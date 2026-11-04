use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::Read;
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
            if unsafe { libc::mkdirat(current.as_raw_fd(), component_name.as_ptr(), 0o755) } < 0 {
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
    // falls back to local approval.
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
