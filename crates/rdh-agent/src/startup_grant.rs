//! Administrator-owned, per-config startup grants; never trusted from the user profile.
use std::path::Path;

use crate::error::{Error, Result};
use crate::startup::Mode;
#[cfg(any(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-startup"),
        all(windows, feature = "experimental-windows-startup")
    )
))]
use serde_json::Value;

#[cfg(any(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-startup"),
        all(windows, feature = "experimental-windows-startup")
    )
))]
pub(crate) fn permits(raw: &[u8], config: &Path, mode: Mode) -> Result<bool> {
    if raw.len() > 4096 {
        return Err(Error::message("Managed startup grant is too large"));
    }
    let value: Value = serde_json::from_slice(raw)?;
    Ok(value.get("version").and_then(Value::as_u64) == Some(1)
        && value.get("config").and_then(Value::as_str) == config.to_str()
        && value
            .get("modes")
            .and_then(Value::as_array)
            .is_some_and(|modes| {
                modes.iter().any(|item| {
                    item.as_str()
                        == Some(match mode {
                            Mode::None => "none",
                            Mode::User => "user",
                            Mode::Service => "service",
                        })
                })
            }))
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup")
))]
mod posix {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::config::sha256_hex;

    fn trusted(file: &File, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        if (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(Error::message("Untrusted managed startup grant"));
        }
        Ok(())
    }
    fn name(value: &OsStr) -> Result<CString> {
        CString::new(value.as_bytes()).map_err(|_| Error::message("Invalid grant path"))
    }
    fn open_at(parent: &File, value: &OsStr, flags: i32, mode: u32) -> Result<File> {
        let value = name(value)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                value.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn directory(create: bool) -> Result<File> {
        let mut current = File::open("/")?;
        trusted(&current, true)?;
        #[cfg(target_os = "macos")]
        let components = ["private", "etc", "remote-device-hub", "startup-grants"].as_slice();
        #[cfg(not(target_os = "macos"))]
        let components = ["etc", "remote-device-hub", "startup-grants"].as_slice();
        for component in components {
            if create {
                let component_name = CString::new(*component).unwrap();
                if unsafe { libc::mkdirat(current.as_raw_fd(), component_name.as_ptr(), 0o755) } < 0
                {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                }
            }
            current = open_at(
                &current,
                OsStr::new(component),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )?;
            trusted(&current, true)?;
        }
        Ok(current)
    }
    fn grant_name(config: &Path) -> Result<String> {
        let config = config
            .to_str()
            .ok_or_else(|| Error::message("Startup config path is not Unicode"))?;
        Ok(format!("{}.json", sha256_hex(config.as_bytes())))
    }
    fn absent(error: &Error) -> bool {
        matches!(error, Error::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
    }
    pub(crate) fn managed_grant(config: &Path, mode: Mode) -> Result<bool> {
        let filename = grant_name(config)?;
        // Absence never authorizes anything. Check it before walking the trust
        // chain so an unprivileged namespace without host root ownership still
        // falls back to local approval, as the previous Agent did.
        #[cfg(target_os = "macos")]
        let grant_directory = "/private/etc/remote-device-hub/startup-grants";
        #[cfg(not(target_os = "macos"))]
        let grant_directory = "/etc/remote-device-hub/startup-grants";
        let full_path = Path::new(grant_directory).join(&filename);
        match std::fs::symlink_metadata(full_path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let directory = match directory(false) {
            Ok(directory) => directory,
            Err(error) if absent(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let file = match open_at(
            &directory,
            OsStr::new(&filename),
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        ) {
            Ok(file) => file,
            Err(error) if absent(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        trusted(&file, false)?;
        if file.metadata()?.len() > 4096 {
            return Err(Error::message("Managed startup grant is too large"));
        }
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        permits(&bytes, config, mode)
    }
    pub(crate) fn set_grant(config: &Path, mode: Mode) -> Result<()> {
        if unsafe { libc::getuid() } != 0 {
            return Err(Error::message(
                "Administrator privileges are required to grant managed startup",
            ));
        }
        let directory = directory(true)?;
        let filename = grant_name(config)?;
        match open_at(
            &directory,
            OsStr::new(&filename),
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        ) {
            Ok(file) => trusted(&file, false)?,
            Err(error) if absent(&error) => {}
            Err(error) => return Err(error),
        }
        let filename_c = CString::new(filename).unwrap();
        if mode == Mode::None {
            if unsafe { libc::unlinkat(directory.as_raw_fd(), filename_c.as_ptr(), 0) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error.into());
                }
            }
            crate::storage::sync_directory(&directory)?;
            return Ok(());
        }
        let temp = format!("{}.tmp", crate::random::hex_token::<16>()?);
        let temp_c = CString::new(temp.clone()).unwrap();
        let mut created = false;
        let result = (|| -> Result<()> {
            let mut file = open_at(
                &directory,
                OsStr::new(&temp),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o644,
            )?;
            created = true;
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o644) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            trusted(&file, false)?;
            let value = serde_json::json!({"version":1,"config":config.to_str(),"modes":[mode]});
            serde_json::to_writer(&mut file, &value)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temp_c.as_ptr(),
                    directory.as_raw_fd(),
                    filename_c.as_ptr(),
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            created = false;
            crate::storage::sync_directory(&directory)?;
            Ok(())
        })();
        if created {
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), temp_c.as_ptr(), 0);
            }
        }
        result
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup")
))]
pub(crate) use posix::{managed_grant, set_grant};
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub(crate) fn managed_grant(_: &Path, _: Mode) -> Result<bool> {
    Err(Error::message(
        "Managed startup grants are unsupported on this platform",
    ))
}
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub(crate) fn set_grant(_: &Path, _: Mode) -> Result<()> {
    Err(Error::message(
        "Managed startup grants are unsupported on this platform",
    ))
}

#[cfg(all(windows, feature = "experimental-windows-startup"))]
pub(crate) use crate::storage::windows::startup_grants::{managed_grant, set_grant};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grants_are_bound_to_the_exact_config_and_mode() {
        let raw = br#"{"version":1,"config":"/tmp/config.json","modes":["user"]}"#;
        assert!(permits(raw, Path::new("/tmp/config.json"), Mode::User).unwrap());
        assert!(!permits(raw, Path::new("/tmp/config.json"), Mode::Service).unwrap());
        assert!(!permits(raw, Path::new("/tmp/other.json"), Mode::User).unwrap());
        assert!(permits(&[b' '; 4097], Path::new("/tmp/config.json"), Mode::User).is_err());
    }
}
