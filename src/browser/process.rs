//! Browser ownership and profile lifetime shared by all native launchers.
use super::{Failure, Result, cdp::Cdp, io_error, profiles::NamedProfile};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[cfg(unix)]
mod posix;
#[cfg(unix)]
use posix as platform;
#[cfg(windows)]
mod windows;
pub use platform::detect;
#[cfg(windows)]
use windows as platform;
#[cfg(windows)]
pub(super) use windows::supervisor::run as run_supervisor;

pub struct BrowserProcess {
    child: platform::Child,
    pub cdp: Cdp,
    pub profile: PathBuf,
    closed: bool,
    named: Option<NamedProfile>,
    owner: Value,
}

fn arguments(data: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--headless",
        "--remote-debugging-pipe",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--site-per-process",
        "--window-size=1280,720",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let mut directory = OsString::from("--user-data-dir=");
    directory.push(data);
    args.push(directory);
    args.push("about:blank".into());
    args
}

impl BrowserProcess {
    pub async fn launch(
        executable: &Path,
        root: &Path,
        id: &str,
        named: Option<NamedProfile>,
        deadline: Instant,
    ) -> Result<Self> {
        let profile = root.join(id);
        crate::storage::create_private_dir(&profile).map_err(io_error)?;
        let args = arguments(
            named
                .as_ref()
                .map_or(profile.as_path(), |p| p.data.as_path()),
        );
        let (child, read, write) = match platform::Child::spawn(executable, &profile, &args) {
            Ok(value) => value,
            Err(error) => {
                let _ = fs::remove_dir_all(&profile);
                return Err(io_error(error));
            }
        };
        let mut owner = child.owner();
        owner["profile_id"] = named
            .as_ref()
            .and_then(|p| p.data.parent()?.file_name()?.to_str())
            .map_or(Value::Null, |s| s.into());
        let mut process = Self {
            child,
            cdp: Cdp::new(read, write),
            profile,
            closed: false,
            named,
            owner,
        };
        // Publish ownership before a suspended platform child can start, and
        // retain this record until the complete owned process tree has exited.
        let owner = &process.owner;
        if owner["start"].as_str().is_none_or(str::is_empty)
            || crate::storage::write_private_new(
                &process.profile.join("owner.json"),
                &serde_json::to_vec(&owner).unwrap(),
            )
            .is_err()
            || process.child.start().is_err()
        {
            let _ = process.close().await;
            return Err(Failure::unknown("browser_start_failed"));
        }
        let ready = process
            .cdp
            .call(None, "Browser.getVersion", json!({}), deadline)
            .await;
        if ready.is_err() {
            let _ = process.close().await;
        }
        ready?;
        Ok(process)
    }

    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.cdp.graceful_stop();
        let graceful = Instant::now() + Duration::from_secs(2);
        let deadline = graceful + Duration::from_secs(1);
        loop {
            if self
                .child
                .stopped()
                .map_err(|_| Failure::unknown("browser_cleanup_pending"))?
            {
                break;
            }
            if Instant::now() >= graceful {
                self.child.force_stop();
            }
            if Instant::now() >= deadline {
                return Err(Failure::unknown("browser_cleanup_pending"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Publish completion before deleting data. A retirement name preserves
        // that fact even if an interrupted deletion removes the owner record.
        self.owner["retired"] = true.into();
        if self.profile.try_exists().map_err(cleanup_pending)? {
            crate::storage::write_private_replace(
                &self.profile.join("owner.json"),
                &serde_json::to_vec(&self.owner).unwrap(),
            )
            .map_err(io_error)?;
        }
        retire(&self.profile)?;
        self.closed = true;
        self.named = None;
        Ok(())
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if !self.closed {
            if let Some(named) = self.named.as_mut() {
                named.retain();
            }
            self.cdp.stop();
            self.child.force_stop();
            for _ in 0..20 {
                if matches!(self.child.stopped(), Ok(true)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn cleanup_pending(_: impl std::fmt::Display) -> Failure {
    Failure::new("browser_cleanup_pending")
}

/// The root exclusion lock protects this transition. The renamed directory is
/// a durable cleanup state: no browser or witness may still use it. Windows can
/// briefly deny deletion even after process completion; retry without losing
/// the completion proof or mistaking partially deleted metadata for live work.
fn retire(path: &Path) -> Result<()> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap();
    let retired = path.with_file_name(format!("retired-{name}"));
    if path.try_exists().map_err(cleanup_pending)? {
        if retired.try_exists().map_err(cleanup_pending)? {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        fs::rename(path, &retired).map_err(cleanup_pending)?;
        #[cfg(unix)]
        crate::storage::sync_directory(
            &fs::File::open(path.parent().unwrap()).map_err(cleanup_pending)?,
        )
        .map_err(cleanup_pending)?;
    }
    if retired.try_exists().map_err(cleanup_pending)? {
        fs::remove_dir_all(retired).map_err(cleanup_pending)?;
    }
    Ok(())
}

/// Recovery observes native ownership; it never adopts a page or replays input.
pub fn recover(root: &Path) -> Result<()> {
    let entries = fs::read_dir(root)
        .map_err(io_error)?
        .take(20)
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(io_error)?;
    if entries.len() > 19 {
        return Err(Failure::new("browser_cleanup_pending"));
    }
    for entry in entries {
        let id = entry.file_name();
        let id = id
            .to_str()
            .ok_or_else(|| Failure::new("browser_cleanup_pending"))?;
        if matches!(id, "profiles" | "artifacts") {
            crate::storage::private_metadata(&entry.path(), true).map_err(io_error)?;
            continue;
        }
        let original_id = id.strip_prefix("retired-").unwrap_or(id);
        if original_id.len() != 32 || !original_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        let path = entry.path();
        crate::storage::private_metadata(&path, true).map_err(io_error)?;
        if original_id != id {
            fs::remove_dir_all(path).map_err(cleanup_pending)?;
            continue;
        }
        let owner = match crate::storage::read_private(&path.join("owner.json")) {
            Ok(bytes) => Some(serde_json::from_slice::<Value>(&bytes).map_err(io_error)?),
            Err(crate::error::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                None
            }
            Err(error) => return Err(io_error(error)),
        };
        // The run directory is made before any process setup. Remove only an
        // atomically empty directory; a racing ownership publication blocks it.
        if owner.is_none() && fs::read_dir(&path).map_err(io_error)?.next().is_none() {
            fs::remove_dir(&path).map_err(io_error)?;
            continue;
        }
        if !platform::retired(&path, owner.as_ref())
            .map_err(|_| Failure::new("browser_cleanup_pending"))?
        {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        retire(&path)?;
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn interrupted_retirement_keeps_completion_without_owner_metadata() {
    let root = std::env::temp_dir().join(format!(
        "rdh-retirement-{}",
        crate::random::hex_token::<12>().unwrap()
    ));
    crate::storage::create_private_dir(&root).unwrap();
    let retired = root.join(format!("retired-{}", "a".repeat(32)));
    crate::storage::create_private_dir(&retired).unwrap();
    crate::storage::write_private_new(&retired.join("browser.stderr"), b"remaining data").unwrap();
    recover(&root).unwrap();
    assert!(!retired.exists());
    fs::remove_dir(root).unwrap();
}
