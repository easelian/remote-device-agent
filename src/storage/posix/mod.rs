use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::MAX_PRIVATE_FILE_BYTES;
use crate::error::{Error, Result};
use crate::random;

pub fn verify_metadata(metadata: &Metadata, directory: bool) -> Result<()> {
    let correct_kind = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    // SAFETY: getuid has no preconditions and cannot mutate Rust memory.
    let uid = unsafe { libc::getuid() };
    if !correct_kind || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(Error::message(
            "Private storage must be owned by the current user with no group/other access",
        ));
    }
    Ok(())
}

pub fn private_metadata(path: &Path, directory: bool) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::message("Private storage cannot be a symbolic link"));
    }
    verify_metadata(&metadata, directory)?;
    Ok(metadata)
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("A private storage parent directory is required"))?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    Ok(())
}

pub fn create_private_dir(path: &Path) -> Result<Metadata> {
    DirBuilder::new().mode(0o700).create(path)?;
    private_metadata(path, true)
}

pub fn read_private(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    verify_metadata(&metadata, false)?;
    let path_metadata = private_metadata(path, false)?;
    if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
        return Err(Error::message(
            "Private storage changed while it was opened",
        ));
    }
    if metadata.len() > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    let mut contents = Vec::new();
    file.take(MAX_PRIVATE_FILE_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    Ok(contents)
}

struct Temporary {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.is_file()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn write_private_new(path: &Path, data: &[u8]) -> Result<()> {
    if data.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("A private storage parent directory is required"))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::message("A private storage filename is required"))?;
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(".{}.tmp", random::hex_token::<16>()?));
    let temporary_path = parent.join(temporary_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary_path)?;
    let metadata = file.metadata()?;
    verify_metadata(&metadata, false)?;
    let temporary = Temporary {
        path: temporary_path,
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    file.write_all(data)?;
    file.sync_all()?;
    let current = private_metadata(&temporary.path, false)?;
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(Error::message(
            "Private temporary file changed during creation",
        ));
    }
    // link() is atomic and fails if the destination exists; unlike rename(),
    // it cannot overwrite an existing pairing created by another Agent.
    fs::hard_link(&temporary.path, path)?;
    drop(file);
    drop(temporary);
    if let Err(error) = File::open(parent).and_then(|directory| super::sync_directory(&directory)) {
        return Err(Error::message(format!(
            "Configuration was written, but its directory could not be synchronized: {error}"
        )));
    }
    Ok(())
}

/// Replace a private sidecar under the caller's cross-process lock. The
/// temporary file is durable before rename; a new path is installed with
/// exclusive link semantics so an unexpected existing file is not lost.
pub fn write_private_replace(path: &Path, data: &[u8]) -> Result<()> {
    if data.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(Error::message(
            "Private storage file exceeds the size limit",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::message("A private storage parent directory is required"))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::message("A private storage filename is required"))?;
    let existing = match private_metadata(path, false) {
        Ok(metadata) => Some(metadata),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(".{}.tmp", random::hex_token::<16>()?));
    let temporary_path = parent.join(temporary_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary_path)?;
    let metadata = file.metadata()?;
    verify_metadata(&metadata, false)?;
    let temporary = Temporary {
        path: temporary_path,
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    file.write_all(data)?;
    file.sync_all()?;
    let current = private_metadata(&temporary.path, false)?;
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(Error::message(
            "Private temporary file changed during creation",
        ));
    }
    if let Some(existing) = existing {
        let current = private_metadata(path, false)?;
        if current.dev() != existing.dev() || current.ino() != existing.ino() {
            return Err(Error::message(
                "Private sidecar changed while being updated",
            ));
        }
        fs::rename(&temporary.path, path)?;
    } else {
        fs::hard_link(&temporary.path, path)?;
    }
    drop(file);
    drop(temporary);
    super::sync_directory(&File::open(parent)?)?;
    Ok(())
}
