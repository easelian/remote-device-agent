//! POSIX descriptors and an unreaped process-group leader; no desktop input.
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child as Process, Command, Stdio},
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as group;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as group;

pub struct Child {
    process: Process,
    identity: String,
    reaped: bool,
}

fn duplicate(fd: i32) -> io::Result<OwnedFd> {
    // Keep sources away from Chromium's required descriptors 3 and 4.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}
fn exited(pid: u32) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}
impl Child {
    pub fn spawn(
        executable: &Path,
        profile: &Path,
        args: &[OsString],
    ) -> io::Result<(Self, tokio::net::UnixStream, tokio::net::UnixStream)> {
        let (input, browser_input) = UnixStream::pair()?;
        let (output, browser_output) = UnixStream::pair()?;
        input.set_nonblocking(true)?;
        output.set_nonblocking(true)?;
        let input = tokio::net::UnixStream::from_std(input)?;
        let output = tokio::net::UnixStream::from_std(output)?;
        let child_input = duplicate(browser_input.as_raw_fd())?;
        let child_output = duplicate(browser_output.as_raw_fd())?;
        let (read, write) = (child_input.as_raw_fd(), child_output.as_raw_fd());
        group::prepare()?;
        let stderr = crate::storage::create_private_file(&profile.join("browser.stderr"))
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut command = Command::new(executable);
        command
            .args(args)
            .env_clear()
            .env("HOME", profile)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .current_dir(profile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0);
        #[cfg(target_os = "macos")]
        if let Some(home) = std::env::var_os("HOME") {
            // Security.framework resolves the user's login Keychain through
            // the OS home. Browser data remains in the explicit user-data-dir.
            command.env("HOME", home);
        }
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(read, 3) < 0 || libc::dup2(write, 4) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = command.spawn()?;
        // From this point onward return the owner, including when birth lookup
        // fails: common launch will stop it without losing its profile lease.
        let identity = crate::process_identity::started(process.id())
            .ok()
            .flatten()
            .unwrap_or_default();
        Ok((
            Self {
                process,
                identity,
                reaped: false,
            },
            output,
            input,
        ))
    }
    pub fn owner(&self) -> Value {
        json!({"version":1,"pid":self.process.id(),"start":self.identity})
    }
    pub fn start(&mut self) -> io::Result<()> {
        Ok(())
    }
    pub fn force_stop(&mut self) {
        if !self.reaped && exited(self.process.id()).is_ok() {
            // waitid(WNOWAIT) proves the child identity is still pinned.
            unsafe {
                libc::kill(-(self.process.id() as i32), libc::SIGKILL);
            }
        }
    }
    pub fn stopped(&mut self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        group::reap(self.process.id());
        if !exited(self.process.id())? || !group::stopped(self.process.id())? {
            return Ok(false);
        }
        // A zombie leader cannot block wait; only now release its PID pin.
        self.process.wait()?;
        self.reaped = true;
        Ok(true)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            self.force_stop();
            let _ = self.process.try_wait();
        }
    }
}
pub fn retired(owner: &Value) -> io::Result<bool> {
    let pid = owner["pid"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 1 && *v <= i32::MAX as u32)
        .ok_or_else(|| io::Error::other("Invalid browser process owner"))?;
    group::retired(pid)
}
pub fn detect(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_owned());
    }
    #[cfg(target_os = "linux")]
    let paths = [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/opt/google/chrome/chrome",
    ];
    #[cfg(target_os = "macos")]
    let paths = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    ];
    paths.iter().map(PathBuf::from).find(|p| p.is_file())
}
