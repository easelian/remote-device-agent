//! Root-owned program storage. No user configuration is read through this API.
use crate::{
    error::{Error, Result},
    storage,
};
use std::{
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path},
};

pub(super) fn root() -> Result<()> {
    if unsafe { libc::getuid() } != 0 || unsafe { libc::geteuid() } != 0 {
        return Err(Error::message(
            "System service installation and offline updates require root",
        ));
    }
    Ok(())
}
fn metadata(path: &Path, directory: bool, sticky_parent: bool) -> Result<Metadata> {
    let m = fs::symlink_metadata(path)?;
    if m.uid() != 0
        || (directory && !m.is_dir())
        || (!directory && (!m.is_file() || m.nlink() != 1))
        || (m.mode() & 0o022 != 0 && !(directory && sticky_parent && m.mode() & 0o1000 != 0))
    {
        return Err(Error::message(format!(
            "Service program storage must be root-owned and protected: {}",
            path.display()
        )));
    }
    #[cfg(target_os = "macos")]
    crate::storage::macos_acl::check(path)?;
    Ok(m)
}
pub(super) fn directory(path: &Path) -> Result<()> {
    super::path_text(path)?;
    let mut current = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::Normal(_) => current.push(part),
            _ => return Err(Error::message("Service program paths must be normalized")),
        }
        metadata(&current, true, current != path)?;
    }
    Ok(())
}
pub(super) fn parent_for_new(path: &Path) -> Result<std::path::PathBuf> {
    super::path_text(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("Service program directory needs an existing parent"))?
        .canonicalize()?;
    // A root-owned sticky directory is safe for creation of a root-owned child.
    let mut current = std::path::PathBuf::new();
    for part in parent.components() {
        current.push(part);
        metadata(&current, true, true)?;
    }
    let name = path
        .file_name()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::message("Invalid service program directory"))?;
    let destination = parent.join(name);
    match fs::symlink_metadata(&destination) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(destination),
        Err(e) => Err(e.into()),
        Ok(_) => Err(Error::message(
            "Service installation already exists; it was not overwritten",
        )),
    }
}
pub(super) fn create_directory(path: &Path) -> Result<()> {
    DirBuilder::new().mode(0o755).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    directory(path)?;
    sync(
        path.parent()
            .ok_or_else(|| Error::message("Missing program parent"))?,
    )
}
pub(super) fn sync(path: &Path) -> Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    storage::sync_directory(&directory)?;
    Ok(())
}
pub(super) fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = open(path)?;
    if file.metadata()?.len() > limit {
        return Err(Error::message(
            "Service program record exceeds its size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::message(
            "Service program record exceeds its size limit",
        ));
    }
    Ok(bytes)
}
pub(super) fn open(path: &Path) -> Result<File> {
    directory(
        path.parent()
            .ok_or_else(|| Error::message("Missing program parent"))?,
    )?;
    let expected = metadata(path, false, false)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let held = file.metadata()?;
    if held.dev() != expected.dev() || held.ino() != expected.ino() {
        return Err(Error::message(
            "Service program storage changed while opening",
        ));
    }
    Ok(file)
}
pub(super) fn create(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    root()?;
    directory(
        path.parent()
            .ok_or_else(|| Error::message("Missing program parent"))?,
    )?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if executable { 0o755 } else { 0o644 })
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    // Respect no caller umask that would prevent the service user reading it.
    if unsafe { libc::fchmod(file.as_raw_fd(), if executable { 0o755 } else { 0o644 }) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    metadata(path, false, false)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    sync(path.parent().unwrap())
}
pub(super) fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("Missing program parent"))?;
    let temporary = parent.join(format!(".record-{}.tmp", crate::random::hex_token::<16>()?));
    create(&temporary, bytes, false)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    sync(parent)
}
pub(super) struct Lock(File);
impl Lock {
    pub fn acquire(path: &Path, exclusive: bool) -> Result<Self> {
        if exclusive {
            root()?;
        }
        let file = open(path)?;
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                (if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(Error::message(
                "A service Agent or installation manager is active; stop the service before updating",
            ));
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
