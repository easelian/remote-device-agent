//! Shared POSIX mechanics for bounded startup commands and registration files.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

pub(super) fn choose_stable_executable(
    candidates: impl IntoIterator<Item = PathBuf>,
    actual: &Path,
) -> Result<PathBuf> {
    let expected = fs::metadata(actual)?;
    for candidate in candidates {
        if !candidate.is_absolute()
            || candidate.starts_with("/proc")
            || candidate.starts_with("/dev/fd")
        {
            continue;
        }
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
            && metadata.dev() == expected.dev()
            && metadata.ino() == expected.ino()
        {
            // Keep installation symlinks in the path: resolving them here
            // would pin startup to the old version after an upgrade.
            return Ok(candidate);
        }
    }
    Ok(actual.to_path_buf())
}

pub(super) fn stable_executable() -> Result<PathBuf> {
    let actual = std::env::current_exe()?.canonicalize()?;
    let mut candidates = Vec::new();
    if let Some(argument) = std::env::args_os().next() {
        let argument = PathBuf::from(argument);
        if argument.components().count() > 1 || argument.is_absolute() {
            candidates.push(if argument.is_absolute() {
                argument
            } else {
                std::env::current_dir()?.join(argument)
            });
        } else if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                let directory = if directory.is_absolute() {
                    directory
                } else {
                    std::env::current_dir()?.join(directory)
                };
                candidates.push(directory.join(&argument));
            }
        }
    }
    choose_stable_executable(candidates, &actual)
}

/// Fixed capture and wall-time budgets also apply while output is flowing.
/// Overflow is an error, so callers never infer state from a truncated list.
pub(super) fn run(
    command: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<(ExitStatus, String)> {
    let mut command = Command::new(command);
    command.args(args);
    run_command(command, timeout)
}

pub(crate) fn run_command(mut command: Command, timeout: Duration) -> Result<(ExitStatus, String)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut output = child
            .stdout
            .take()
            .ok_or_else(|| Error::message("Startup manager output is unavailable"))?;
        // SAFETY: output owns a live pipe and these fcntl operations only
        // read/change its descriptor flags.
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(Error::message("Could not configure startup manager output"));
        }
        let mut captured = Vec::new();
        let mut bytes = [0u8; 4096];
        let expires = Instant::now() + timeout;
        let mut exited = None;
        loop {
            loop {
                if Instant::now() >= expires {
                    return Err(Error::message(
                        "Startup manager timed out; inspect native state before retrying",
                    ));
                }
                match output.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        if captured.len() + count > 65536 {
                            return Err(Error::message(
                                "Startup manager output exceeds the limit; inspect native state before retrying",
                            ));
                        }
                        captured.extend_from_slice(&bytes[..count]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            if let Some(status) = exited {
                return Ok((status, String::from_utf8_lossy(&captured).trim().to_owned()));
            }
            if let Some(status) = child.try_wait()? {
                exited = Some(status);
                continue;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        // Only the manager command is stopped, never the managed Agent.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

pub(super) struct Created {
    path: PathBuf,
    device: u64,
    inode: u64,
    pub(super) retain: bool,
}
impl Created {
    pub(super) fn write(path: &Path, contents: &[u8], mode: u32) -> Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        let created = Self {
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
            retain: false,
        };
        if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(created)
    }
}
impl Drop for Created {
    fn drop(&mut self) {
        if !self.retain
            && let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.is_file()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_overflow_and_timeout_are_bounded_and_never_return_partial_success() {
        let started = Instant::now();
        let error = run(
            Path::new("/bin/sh"),
            &[
                "-c",
                "while :; do printf '0123456789012345678901234567890123456789'; done",
            ],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exceeds the limit"));
        assert!(started.elapsed() < Duration::from_secs(5));
        let started = Instant::now();
        let error = run(Path::new("/bin/sleep"), &["10"], Duration::from_millis(40)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
        let (status, text) = run(
            Path::new("/bin/sh"),
            &["-c", "printf 'complete\\n'; exit 7"],
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(text, "complete");
    }
    #[test]
    fn user_startup_preserves_a_verified_version_pointer() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "rdh-startup-pointer-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let old = root.join("old");
        let new = root.join("new");
        // This tests pointer selection, not executable creation. Existing
        // programs avoid ETXTBSY on a freshly written script during fork-heavy
        // parallel tests. Selecting the old shell would not print this argument.
        symlink("/bin/sh", &old).unwrap();
        symlink("/bin/echo", &new).unwrap();
        let pointer = root.join("rdh");
        symlink("old", &pointer).unwrap();
        let selected = choose_stable_executable([pointer.clone()], &old).unwrap();
        assert_eq!(selected, pointer);
        fs::remove_file(&pointer).unwrap();
        symlink("new", &pointer).unwrap();
        let output = Command::new(selected).arg("new").output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "new\n");
        fs::remove_dir_all(root).unwrap();
    }
}
