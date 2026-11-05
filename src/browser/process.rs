//! Owned Linux process groups and private profiles. Never searches by executable name.
use super::{Failure, Result, cdp::Cdp, io_error, profiles::NamedProfile};
use serde_json::{Value, json};
use std::{
    fs,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct BrowserProcess {
    child: Child,
    pub cdp: Cdp,
    pub profile: PathBuf,
    pid: i32,
    identity: String,
    closed: bool,
    named: Option<NamedProfile>,
}

fn process_stat(pid: i32) -> Option<(i32, char, String)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = text.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((
        fields.get(2)?.parse().ok()?,
        fields.first()?.chars().next()?,
        fields.get(19)?.to_string(),
    ))
}

fn group_members(group: i32) -> Vec<(i32, char)> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let (pgrp, state, _) = process_stat(pid)?;
            (pgrp == group).then_some((pid, state))
        })
        .collect()
}

fn reap(group: i32) {
    for (pid, _) in group_members(group) {
        if pid != group {
            // SAFETY: only a known child can be reaped; never reaps other process jobs.
            unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
    }
}

fn duplicate(fd: i32) -> std::io::Result<OwnedFd> {
    // Keep the source descriptors away from Chromium's destinations 3 and 4.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copy < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

pub fn detect(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_owned());
    }
    [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/opt/google/chrome/chrome",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
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
        let result = Self::spawn(executable, &profile, named);
        let mut process = match result {
            Ok(process) => process,
            Err(error) => {
                let _ = fs::remove_dir_all(&profile);
                return Err(error);
            }
        };
        // Keep the process owner available until cleanup is confirmed. An error
        // after spawn must not erase the only directory that restart recovery
        // can use to block reuse of a possibly live named profile.
        let metadata = json!({"version":1,"pid":process.pid,"start":process.identity});
        if process.identity.is_empty()
            || crate::storage::write_private_new(
                &profile.join("owner.json"),
                &serde_json::to_vec(&metadata).unwrap(),
            )
            .is_err()
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

    fn spawn(executable: &Path, profile: &Path, named: Option<NamedProfile>) -> Result<Self> {
        let (input, browser_input) = UnixStream::pair().map_err(io_error)?;
        let (output, browser_output) = UnixStream::pair().map_err(io_error)?;
        // Finish fallible parent-side setup before a child exists.
        input.set_nonblocking(true).map_err(io_error)?;
        output.set_nonblocking(true).map_err(io_error)?;
        let input = tokio::net::UnixStream::from_std(input).map_err(io_error)?;
        let output = tokio::net::UnixStream::from_std(output).map_err(io_error)?;
        let child_input = duplicate(browser_input.as_raw_fd()).map_err(io_error)?;
        let child_output = duplicate(browser_output.as_raw_fd()).map_err(io_error)?;
        let in_fd = child_input.as_raw_fd();
        let out_fd = child_output.as_raw_fd();
        // Reap only owned group members below; other Agent job owners retain their wait status.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let stderr = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(profile.join("browser.stderr"))
            .map_err(io_error)?;
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("HOME", profile)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .args([
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
            ])
            .arg(format!(
                "--user-data-dir={}",
                named
                    .as_ref()
                    .map_or(profile, |p| p.data.as_path())
                    .display()
            ))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                // Async-signal-safe calls only. dup2 clears CLOEXEC for the two pipe endpoints.
                if libc::dup2(in_fd, 3) < 0 || libc::dup2(out_fd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(io_error)?;
        drop(child_input);
        drop(child_output);
        drop(browser_input);
        drop(browser_output);
        let pid = child.id() as i32;
        let identity = process_stat(pid).map(|v| v.2).unwrap_or_default();
        let cdp = Cdp::new(output, input);
        Ok(Self {
            child,
            cdp,
            profile: profile.into(),
            pid,
            identity,
            closed: false,
            named,
        })
    }

    fn force_stop(&mut self) {
        self.cdp.stop();
        if process_stat(self.pid)
            .is_some_and(|(group, _, start)| group == self.pid && start == self.identity)
        {
            // The recorded leader still anchors this group. A reused PID never qualifies.
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        } else if matches!(self.child.try_wait(), Ok(None)) {
            // An unreaped child cannot have its PID reused, even when /proc is unavailable.
            let _ = self.child.kill();
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let _ = self
            .cdp
            .call(
                None,
                "Browser.close",
                json!({}),
                Instant::now() + Duration::from_millis(500),
            )
            .await;
        self.cdp.stop();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            reap(self.pid);
            if group_members(self.pid)
                .iter()
                .all(|(_, state)| *state == 'Z')
            {
                break;
            }
            if Instant::now() >= deadline {
                self.force_stop();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for _ in 0..100 {
            let _ = self.child.try_wait();
            reap(self.pid);
            if group_members(self.pid)
                .iter()
                .all(|(_, state)| *state == 'Z')
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if group_members(self.pid)
            .iter()
            .any(|(_, state)| *state != 'Z')
        {
            return Err(Failure::unknown("browser_cleanup_pending"));
        }
        fs::remove_dir_all(&self.profile).map_err(io_error)?;
        self.closed = true;
        self.named = None;
        Ok(())
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if !self.closed {
            // Reusing a profile requires proven process termination. If normal
            // close failed, keep its reservation until restart recovery checks
            // the recorded process group; a best-effort Drop kill is insufficient.
            if let Some(named) = self.named.as_mut() {
                named.retain();
            }
            self.force_stop();
            // Drop must not block an Agent worker forever on an uninterruptible child.
            for _ in 0..20 {
                if !matches!(self.child.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            reap(self.pid);
        }
    }
}

/// Restart recovery never adopts a browser or resumes an action. Unknown ownership is retained.
pub fn recover(root: &Path) -> Result<()> {
    crate::storage::private_metadata(root, true).map_err(io_error)?;
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
        let Some(id) = id.to_str() else {
            return Err(Failure::new("browser_cleanup_pending"));
        };
        if matches!(id, "profiles" | "artifacts" | "agent.rdh-lock") {
            crate::storage::private_metadata(&entry.path(), true).map_err(io_error)?;
            continue;
        }
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        let path = entry.path();
        crate::storage::private_metadata(&path, true).map_err(io_error)?;
        let bytes = crate::storage::read_private(&path.join("owner.json")).map_err(io_error)?;
        let metadata: Value = serde_json::from_slice(&bytes).map_err(io_error)?;
        let pid = metadata["pid"]
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v > 1)
            .ok_or_else(|| Failure::new("browser_cleanup_pending"))?;
        // Pipe closure normally already stopped it. Do not kill even a matching PID on recovery:
        // another surviving Agent could own it. Existing instance/profile locks are authoritative.
        if group_members(pid).iter().any(|(_, state)| *state != 'Z') {
            return Err(Failure::new("browser_cleanup_pending"));
        }
        fs::remove_dir_all(path).map_err(io_error)?;
    }
    Ok(())
}
