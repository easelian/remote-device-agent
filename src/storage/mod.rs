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
pub(crate) mod windows;

#[cfg(all(target_os = "macos", feature = "experimental-macos-install"))]
#[path = "macos/acl.rs"]
pub(crate) mod macos_acl;

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
pub use windows::{ensure_parent, read_private, write_private_new, write_private_replace};

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::file_identity;

#[cfg(unix)]
#[path = "posix/mod.rs"]
mod platform;

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
