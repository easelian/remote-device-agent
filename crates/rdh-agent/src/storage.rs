#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
use std::path::Path;

#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
use crate::error::{Error, Result};

#[cfg(any(unix, all(windows, feature = "experimental-windows-private-storage")))]
const MAX_PRIVATE_FILE_BYTES: u64 = 1024 * 1024;

/// Apple sync_all uses F_FULLFSYNC, which is intended for regular files.
/// Directory publication uses checked POSIX fsync there, retaining the
/// existing sync_all implementation on other Unix targets.
#[cfg(unix)]
pub(crate) fn sync_directory(directory: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    loop {
        use std::os::fd::AsRawFd;
        // SAFETY: directory owns a live descriptor, borrowed by fsync only.
        if unsafe { libc::fsync(directory.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    #[cfg(not(target_os = "macos"))]
    directory.sync_all()
}

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
#[path = "storage_windows.rs"]
pub(crate) mod windows;

#[cfg(all(target_os = "macos", feature = "experimental-macos-install"))]
#[path = "storage_macos_acl.rs"]
pub(crate) mod macos_acl;

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
pub use windows::{ensure_parent, read_private, write_private_new, write_private_replace};

#[cfg(unix)]
mod platform {
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
        if let Err(error) =
            File::open(parent).and_then(|directory| super::sync_directory(&directory))
        {
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
}

#[cfg(unix)]
pub use platform::{
    create_private_dir, ensure_parent, private_metadata, read_private, write_private_new,
    write_private_replace,
};

#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
fn unsupported() -> Error {
    Error::message("Native private-storage ACL support is not implemented on this platform")
}

#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
pub fn ensure_parent(_: &Path) -> Result<()> {
    Err(unsupported())
}
#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
pub fn read_private(_: &Path) -> Result<Vec<u8>> {
    Err(unsupported())
}
#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
pub fn write_private_new(_: &Path, _: &[u8]) -> Result<()> {
    Err(unsupported())
}
#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
pub fn write_private_replace(_: &Path, _: &[u8]) -> Result<()> {
    Err(unsupported())
}

#[cfg(all(test, windows, not(feature = "experimental-windows-private-storage")))]
mod windows_default_tests {
    #[test]
    fn private_storage_is_unavailable_without_the_experimental_feature() {
        let base = std::env::temp_dir().join(format!(
            "rdh-default-storage-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        std::fs::create_dir(&base).unwrap();
        let existing = base.join("existing.json");
        let missing = base.join("missing").join("config.json");
        std::fs::write(&existing, b"original").unwrap();

        for result in [
            super::ensure_parent(&missing),
            super::read_private(&existing).map(|_| ()),
            super::write_private_new(&missing, b"new"),
            super::write_private_replace(&existing, b"replacement"),
        ] {
            assert!(result.unwrap_err().to_string().contains("not implemented"));
        }
        assert!(!base.join("missing").exists());
        assert_eq!(std::fs::read(&existing).unwrap(), b"original");
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::random;

    #[test]
    fn private_config_creation_never_overwrites_or_reads_a_symlink() {
        let dir = std::env::temp_dir().join(format!(
            "rdh-storage-{}",
            random::hex_token::<12>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let config = dir.join("config.json");
        write_private_new(&config, b"secret").unwrap();
        assert_eq!(read_private(&config).unwrap(), b"secret");
        assert!(write_private_new(&config, b"replace").is_err());
        assert_eq!(read_private(&config).unwrap(), b"secret");
        let alias = dir.join("alias");
        symlink(&config, &alias).unwrap();
        assert!(read_private(&alias).is_err());
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&config).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
