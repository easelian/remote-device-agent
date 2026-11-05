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

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
pub use windows::{
    create_private_dir, create_private_file, ensure_parent, open_private_file, private_metadata,
    read_private, write_private_new, write_private_replace,
};

#[cfg(unix)]
#[path = "posix/mod.rs"]
mod platform;

#[cfg(unix)]
pub use platform::{
    create_private_dir, create_private_file, ensure_parent, open_private_file, private_metadata,
    read_private, write_private_new, write_private_replace,
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

#[cfg(all(
    test,
    any(unix, all(windows, feature = "experimental-windows-private-storage"))
))]
mod stream_tests {
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;

    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rdh-private-stream-{}",
                crate::random::hex_token::<12>().unwrap()
            ));
            create_private_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn streams_support_large_files_and_preserve_contents_when_reopened() {
        let fixture = Fixture::new();
        let path = fixture.0.join("download");
        let contents = vec![0x5a; MAX_PRIVATE_FILE_BYTES as usize + 17];
        let mut created = create_private_file(&path).unwrap();
        created.write_all(&contents).unwrap();
        drop(created);
        assert!(create_private_file(&path).is_err());

        let mut reader = open_private_file(&path, false).unwrap();
        assert!(reader.write_all(b"forbidden").is_err());
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, contents);
        drop(reader);

        let mut writer = open_private_file(&path, true).unwrap();
        let mut prefix = [0; 4];
        writer.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, [0x5a; 4]);
        assert_eq!(
            writer.seek(SeekFrom::End(0)).unwrap(),
            contents.len() as u64
        );
        writer.write_all(b"tail").unwrap();
        drop(writer);
        let mut actual = Vec::new();
        open_private_file(&path, false)
            .unwrap()
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(&actual[..contents.len()], contents);
        assert_eq!(&actual[contents.len()..], b"tail");
        // The configuration API retains its independent size limit.
        assert!(read_private(&path).is_err());
    }

    #[test]
    fn streams_reject_directories_and_multiply_linked_files() {
        let fixture = Fixture::new();
        assert!(open_private_file(&fixture.0, false).is_err());
        let path = fixture.0.join("download");
        drop(create_private_file(&path).unwrap());
        let alias = fixture.0.join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(open_private_file(&path, false).is_err());
        assert!(open_private_file(&alias, true).is_err());
        fs::remove_file(alias).unwrap();
        assert!(open_private_file(&path, true).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn streams_reject_symlinks_and_group_readable_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = Fixture::new();
        let path = fixture.0.join("download");
        drop(create_private_file(&path).unwrap());
        let alias = fixture.0.join("alias");
        symlink(&path, &alias).unwrap();
        assert!(open_private_file(&alias, false).is_err());
        assert!(open_private_file(&alias, true).is_err());
        assert!(create_private_file(&alias).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(open_private_file(&path, false).is_err());
        assert!(open_private_file(&path, true).is_err());
    }
}
