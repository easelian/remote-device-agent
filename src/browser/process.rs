//! Browser ownership and profile lifetime shared by all native launchers.
use super::{
    Failure, Result,
    engine::{Engine, Launch},
    io_error,
    profiles::NamedProfile,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// Test-only helpers for direct Firefox transport and ownership probes.
#[cfg(all(test, target_os = "linux"))]
mod bidi_probe;

#[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
pub(crate) mod firefox;

#[cfg(unix)]
mod posix;
#[cfg(unix)]
use posix as platform;
#[cfg(target_os = "linux")]
pub(super) use posix::run_helper as run_posix_supervisor;
#[cfg(windows)]
mod windows;
pub use super::engine::detect;
#[cfg(windows)]
use windows as platform;
#[cfg(windows)]
pub(super) use windows::supervisor::run as run_supervisor;

#[cfg(feature = "experimental-browser-bidi")]
#[derive(Default)]
pub(crate) struct SessionData {
    pub named: Option<NamedProfile>,
    pub downloads: Option<crate::browser::files::Downloads>,
}

pub struct BrowserProcess {
    child: platform::Child,
    pub engine: Engine,
    pub profile: PathBuf,
    closed: bool,
    named: Option<NamedProfile>,
    owner: Value,
}

impl BrowserProcess {
    pub async fn launch(
        executable: &Path,
        root: &Path,
        id: &str,
        named: Option<NamedProfile>,
        deadline: Instant,
    ) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let owned_root = {
            let path = root.join("chromium-supervised-v1");
            match crate::storage::create_private_dir(&path) {
                Ok(_) => {}
                Err(crate::error::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    crate::storage::private_metadata(&path, true).map_err(io_error)?;
                }
                Err(error) => return Err(io_error(error)),
            }
            path
        };
        #[cfg(target_os = "linux")]
        let root = owned_root.as_path();
        let profile = root.join(id);
        crate::storage::create_private_dir(&profile).map_err(io_error)?;
        let launch = Launch::native(
            named
                .as_ref()
                .map_or(profile.as_path(), |p| p.data.as_path()),
        );
        #[cfg(target_os = "linux")]
        fonts::give(&profile);
        let (child, read, write) = match platform::Child::spawn(executable, &profile, &launch) {
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
            engine: Engine::from_private_channel(read, write),
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
        let ready = process.engine.product(deadline).await;
        if ready.is_err() {
            let _ = process.close().await;
        }
        ready?;
        // The browser has shown nothing but its empty first page so far.
        #[cfg(target_os = "linux")]
        fonts::keep(&process.profile);
        Ok(process)
    }

    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.engine.graceful_stop();
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
        #[cfg(target_os = "linux")]
        if let Some(parent) = self.profile.parent() {
            // An empty family no longer needs to block an older Agent. A
            // concurrent session keeps it present via remove_dir's atomic check.
            let _ = fs::remove_dir(parent);
        }
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
            self.engine.stop();
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

/// Chromium lists every font of the system when its home has no font cache,
/// which takes most of a second, and every session starts in a new home. The
/// cache that the first browser of this process wrote, while it had shown
/// nothing but an empty page, is put into the home of each one started later.
/// It is held in memory, so nothing a later session does can change it, and
/// it is rebuilt whenever the Agent starts.
#[cfg(target_os = "linux")]
mod fonts {
    use std::{
        ffi::OsString,
        fs,
        path::Path,
        sync::{Arc, Mutex},
    };

    const FILES: usize = 256;
    const BYTES: u64 = 16 * 1024 * 1024;
    type Cache = Arc<Vec<(OsString, Vec<u8>)>>;
    static KEPT: Mutex<Option<Cache>> = Mutex::new(None);

    fn directory(home: &Path) -> std::path::PathBuf {
        home.join(".cache").join("fontconfig")
    }

    /// Without the cache the browser only starts slower, so a failure here
    /// is not one of the launch.
    pub(super) fn give(home: &Path) {
        if let Some(kept) = KEPT.lock().unwrap().clone() {
            write(home, &kept);
        }
    }

    pub(super) fn keep(home: &Path) {
        let mut kept = KEPT.lock().unwrap();
        if kept.is_none() {
            *kept = read(home);
        }
    }

    fn write(home: &Path, kept: &Cache) {
        let target = directory(home);
        if crate::storage::create_private_dir(&home.join(".cache")).is_err()
            || crate::storage::create_private_dir(&target).is_err()
        {
            return;
        }
        for (name, bytes) in kept.iter() {
            if crate::storage::write_private_new(&target.join(name), bytes).is_err() {
                let _ = fs::remove_dir_all(&target);
                return;
            }
        }
    }

    fn read(home: &Path) -> Option<Cache> {
        let entries = fs::read_dir(directory(home)).ok()?;
        let mut files = Vec::new();
        let mut total = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            // Only what fontconfig writes: plain files, of a plausible size.
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.is_file() {
                continue;
            }
            total += metadata.len();
            if files.len() == FILES || total > BYTES {
                return None;
            }
            files.push((entry.file_name(), fs::read(&path).ok()?));
        }
        (!files.is_empty()).then(|| Arc::new(files))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_cache_of_one_home_is_written_into_another() {
            let root = std::env::temp_dir().join(format!(
                "rdh-font-cache-test-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            let (first, second) = (root.join("first"), root.join("second"));
            for home in [&first, &second] {
                fs::create_dir_all(home).unwrap();
            }
            assert!(read(&first).is_none());
            fs::create_dir_all(directory(&first)).unwrap();
            fs::write(directory(&first).join("a-le64.cache-9"), b"fonts").unwrap();
            fs::create_dir(directory(&first).join("not-a-file")).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", directory(&first).join("link")).unwrap();
            let kept = read(&first).unwrap();
            assert_eq!(kept.len(), 1);
            write(&second, &kept);
            assert_eq!(
                fs::read(directory(&second).join("a-le64.cache-9")).unwrap(),
                b"fonts"
            );
            assert_eq!(fs::read_dir(directory(&second)).unwrap().count(), 1);
            // A home that already has a cache is left as it is.
            write(&second, &Arc::new(vec![("b".into(), b"other".to_vec())]));
            assert!(!directory(&second).join("b").exists());
            fs::remove_dir_all(root).unwrap();
        }
    }
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
        #[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
        if id == "v2" {
            recover_v2(&entry.path())?;
            continue;
        }
        #[cfg(target_os = "linux")]
        if id == "chromium-supervised-v1" {
            crate::storage::private_metadata(&entry.path(), true).map_err(io_error)?;
            recover(&entry.path())?;
            fs::remove_dir(entry.path()).map_err(cleanup_pending)?;
            continue;
        }
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
        #[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
        let stopped = if owner
            .as_ref()
            .is_some_and(|v| v["kind"] == "linux-bidi-namespace-v1")
        {
            firefox::retired(owner.as_ref().unwrap())
        } else {
            platform::retired(&path, owner.as_ref())
        };
        #[cfg(not(all(target_os = "linux", feature = "experimental-browser-bidi")))]
        let stopped = if owner
            .as_ref()
            .is_some_and(|v| v["kind"] == "linux-bidi-namespace-v1")
        {
            Err(std::io::Error::other(
                "BiDi namespace recovery requires its development feature",
            ))
        } else {
            platform::retired(&path, owner.as_ref())
        };
        if !stopped.map_err(|_| Failure::new("browser_cleanup_pending"))? {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        retire(&path)?;
    }
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
fn recover_v2(root: &Path) -> Result<()> {
    crate::storage::private_metadata(root, true).map_err(io_error)?;
    for entry in fs::read_dir(root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        if !matches!(
            name.to_str(),
            Some("chromium-temporary-v1" | "firefox-temporary-v1")
        ) {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        crate::storage::private_metadata(&entry.path(), true).map_err(io_error)?;
        recover(&entry.path())?;
    }
    Ok(())
}

/// Separate temporary profile families. An old Agent rejects the unknown v2
/// directory instead of adopting or deleting a different browser's files.
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
))]
pub(super) fn prepare_v2(root: &Path, backend: &str) -> Result<PathBuf> {
    fn directory(path: &Path) -> Result<()> {
        match crate::storage::create_private_dir(path) {
            Ok(_) => Ok(()),
            Err(crate::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::storage::private_metadata(path, true)
                    .map(drop)
                    .map_err(io_error)
            }
            Err(e) => Err(io_error(e)),
        }
    }
    let v2 = root.join("v2");
    directory(&v2)?;
    let family = match backend {
        "cdp" => "chromium-temporary-v1",
        "bidi" => "firefox-temporary-v1",
        _ => return Err(Failure::new("browser_selection_invalid")),
    };
    let path = v2.join(family);
    directory(&path)?;
    Ok(path)
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
