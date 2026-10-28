#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
use std::path::PathBuf;

#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
use crate::error::{Error, Result};

#[cfg(unix)]
mod platform {
    use std::fs::{self, File, Metadata, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Serialize};

    use crate::error::{Error, Result};
    use crate::{random, storage};

    #[derive(Clone, Serialize, Deserialize)]
    struct Owner {
        pid: u64,
        nonce: String,
    }

    struct HeldLock {
        path: PathBuf,
        device: u64,
        inode: u64,
        owner: Owner,
    }

    struct Guard {
        path: PathBuf,
        device: u64,
        inode: u64,
    }

    pub struct ProfileLocks {
        held: Vec<HeldLock>,
        release_on_drop: bool,
    }

    fn append(path: &Path, suffix: &str) -> PathBuf {
        let mut result = path.as_os_str().to_os_string();
        result.push(suffix);
        PathBuf::from(result)
    }

    fn lock_path(config: &Path) -> PathBuf {
        append(config, ".node-lock")
    }

    fn owner_error() -> Error {
        Error::message(
            "The device configuration is locked or is being recovered. Confirm the previous Agent has stopped before using --recover-lock.",
        )
    }

    fn same_object(left: &Metadata, device: u64, inode: u64) -> bool {
        left.dev() == device && left.ino() == inode
    }

    fn open_lock_directory(path: &Path) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        let current = storage::private_metadata(path, true)?;
        if !same_object(&current, metadata.dev(), metadata.ino()) {
            return Err(owner_error());
        }
        Ok(file)
    }

    fn read_owner(directory: &File) -> Result<(Owner, Metadata)> {
        // SAFETY: directory owns a valid directory fd, the name is a static NUL-terminated
        // string, and the returned descriptor is uniquely transferred to File.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"owner.json".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a fresh owned file descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        platform_verify_owner(&metadata)?;
        let mut raw = Vec::new();
        file.take(4097).read_to_end(&mut raw)?;
        if raw.len() > 4096 {
            return Err(owner_error());
        }
        let owner: Owner = serde_json::from_slice(&raw)?;
        if owner.pid == 0 || owner.pid > i32::MAX as u64 || owner.nonce.len() < 32 {
            return Err(owner_error());
        }
        Ok((owner, metadata))
    }

    fn platform_verify_owner(metadata: &Metadata) -> Result<()> {
        // SAFETY: getuid has no preconditions and cannot mutate Rust memory.
        let uid = unsafe { libc::getuid() };
        if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(owner_error());
        }
        Ok(())
    }

    fn write_owner(directory: &File, owner: &Owner) -> Result<()> {
        // SAFETY: the directory fd is valid, the name is a static C string, and
        // create-exclusive returns a new descriptor owned by this function.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"owner.json".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                // mode_t is u16 on Darwin and must be promoted for openat's
                // variadic argument. Linux already uses the same width.
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a fresh owned file descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        platform_verify_owner(&file.metadata()?)?;
        serde_json::to_writer(&mut file, owner)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }

    impl Guard {
        fn create(lock: &Path) -> Result<Self> {
            let path = append(lock, ".reclaiming");
            let metadata = storage::create_private_dir(&path).map_err(|error| {
                if matches!(&error, Error::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists)
                {
                    owner_error()
                } else {
                    error
                }
            })?;
            Ok(Self {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if let Ok(metadata) = storage::private_metadata(&self.path, true) {
                if same_object(&metadata, self.device, self.inode) {
                    let _ = fs::remove_dir(&self.path);
                }
            }
        }
    }

    fn sorted_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
        let mut sorted = paths.to_vec();
        sorted.sort();
        sorted.dedup();
        sorted
    }

    impl ProfileLocks {
        pub fn acquire(paths: &[PathBuf]) -> Result<Self> {
            let mut acquired = Self {
                held: Vec::new(),
                release_on_drop: true,
            };
            for config in sorted_paths(paths) {
                let lock = lock_path(&config);
                storage::ensure_parent(&lock)?;
                let guard = Guard::create(&lock)?;
                let metadata = storage::create_private_dir(&lock).map_err(|error| {
                    if matches!(&error, Error::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists) { owner_error() } else { error }
                })?;
                let directory = open_lock_directory(&lock)?;
                if !same_object(&directory.metadata()?, metadata.dev(), metadata.ino()) {
                    return Err(owner_error());
                }
                let owner = Owner {
                    pid: u64::from(std::process::id()),
                    nonce: random::url_token::<32>()?,
                };
                write_owner(&directory, &owner)?;
                acquired.held.push(HeldLock {
                    path: lock,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    owner,
                });
                drop(guard);
            }
            Ok(acquired)
        }

        pub fn retain_on_drop(&mut self) {
            self.release_on_drop = false;
        }

        pub fn release(&mut self) -> Result<()> {
            for item in self.held.iter().rev() {
                let directory = match open_lock_directory(&item.path) {
                    Ok(directory) => directory,
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let metadata = directory.metadata()?;
                if !same_object(&metadata, item.device, item.inode) {
                    return Err(owner_error());
                }
                let (owner, owner_metadata) = read_owner(&directory)?;
                if owner.pid != item.owner.pid || owner.nonce != item.owner.nonce {
                    return Err(owner_error());
                }
                let current_owner =
                    storage::private_metadata(&item.path.join("owner.json"), false)?;
                if !same_object(&current_owner, owner_metadata.dev(), owner_metadata.ino()) {
                    return Err(owner_error());
                }
                // SAFETY: directory is a valid held directory descriptor and the
                // fixed name contains no separators or user-controlled bytes.
                if unsafe { libc::unlinkat(directory.as_raw_fd(), c"owner.json".as_ptr(), 0) } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                let current = storage::private_metadata(&item.path, true)?;
                if !same_object(&current, item.device, item.inode) {
                    return Err(owner_error());
                }
                fs::remove_dir(&item.path)?;
            }
            self.held.clear();
            Ok(())
        }

        pub fn recover(paths: &[PathBuf]) -> Result<()> {
            let paths = sorted_paths(paths);
            let mut guards = Vec::new();
            for config in &paths {
                let lock = lock_path(config);
                storage::ensure_parent(&lock)?;
                guards.push(Guard::create(&lock)?);
            }
            for config in paths {
                let lock = lock_path(&config);
                let directory = match open_lock_directory(&lock) {
                    Ok(directory) => directory,
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let original = directory.metadata()?;
                let (owner, _) = read_owner(&directory)?;
                // SAFETY: pid is validated as a positive pid_t, and signal zero
                // performs an existence/permission check without sending a signal.
                let outcome = unsafe { libc::kill(owner.pid as libc::pid_t, 0) };
                if outcome == 0 {
                    return Err(owner_error());
                }
                let failure = std::io::Error::last_os_error();
                if failure.raw_os_error() != Some(libc::ESRCH) {
                    return Err(failure.into());
                }
                let quarantine = append(&lock, &format!(".stale-{}", random::hex_token::<16>()?));
                fs::rename(&lock, &quarantine)?;
                let moved = storage::private_metadata(&quarantine, true)?;
                if !same_object(&moved, original.dev(), original.ino()) {
                    return Err(owner_error());
                }
                // SAFETY: the original directory fd still refers to the verified
                // quarantined inode; the filename is fixed and relative to it.
                if unsafe { libc::unlinkat(directory.as_raw_fd(), c"owner.json".as_ptr(), 0) } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                fs::remove_dir(quarantine)?;
            }
            drop(guards);
            Ok(())
        }
    }

    impl Drop for ProfileLocks {
        fn drop(&mut self) {
            if self.release_on_drop {
                let _ = self.release();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn fixture() -> (PathBuf, PathBuf) {
            let dir = std::env::temp_dir()
                .join(format!("rdh-lock-{}", random::hex_token::<12>().unwrap()));
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            (dir.clone(), dir.join("config.json"))
        }

        #[test]
        fn exclusive_acquire_and_release_uses_legacy_lock_name() {
            let (dir, config) = fixture();
            let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
            assert!(lock_path(&config).join("owner.json").is_file());
            assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
            assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
            held.release().unwrap();
            assert!(!lock_path(&config).exists());
            drop(ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap());
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn explicit_recovery_reclaims_only_a_proven_dead_owner() {
            let (dir, config) = fixture();
            let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
            let owner = lock_path(&config).join("owner.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&owner).unwrap()).unwrap();
            value["pid"] = serde_json::json!(i32::MAX);
            fs::write(&owner, serde_json::to_vec(&value).unwrap()).unwrap();
            held.retain_on_drop();
            drop(held);
            ProfileLocks::recover(std::slice::from_ref(&config)).unwrap();
            assert!(!lock_path(&config).exists());
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn a_replaced_owner_or_guard_is_never_removed() {
            let (dir, config) = fixture();
            let mut held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
            let owner = lock_path(&config).join("owner.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&owner).unwrap()).unwrap();
            value["nonce"] = serde_json::json!("a".repeat(40));
            fs::write(&owner, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(held.release().is_err());
            held.retain_on_drop();
            drop(held);
            assert!(owner.exists());
            let guard = append(&lock_path(&config), ".reclaiming");
            storage::create_private_dir(&guard).unwrap();
            assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
            assert!(guard.exists());
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(unix)]
pub use platform::ProfileLocks;

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
#[path = "profile_lock_windows.rs"]
mod windows;

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
pub use windows::ProfileLocks;

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::require_dead_owners;

#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
pub struct ProfileLocks;

#[cfg(all(
    not(unix),
    not(all(windows, feature = "experimental-windows-private-storage"))
))]
impl ProfileLocks {
    pub fn acquire(_: &[PathBuf]) -> Result<Self> {
        Err(Error::message(
            "Native profile locks are not implemented on this platform",
        ))
    }
    pub fn recover(_: &[PathBuf]) -> Result<()> {
        Err(Error::message(
            "Native profile locks are not implemented on this platform",
        ))
    }
    pub fn retain_on_drop(&mut self) {}
    pub fn release(&mut self) -> Result<()> {
        Err(Error::message(
            "Native profile locks are not implemented on this platform",
        ))
    }
}

#[cfg(all(test, windows, not(feature = "experimental-windows-private-storage")))]
mod windows_default_tests {
    use super::ProfileLocks;

    #[test]
    fn profile_locks_are_unavailable_without_the_experimental_feature() {
        assert!(ProfileLocks::acquire(&[]).is_err());
        assert!(ProfileLocks::recover(&[]).is_err());
        let base = std::env::temp_dir().join(format!(
            "rdh-default-lock-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        std::fs::create_dir(&base).unwrap();
        let config = base.join("config.json");
        std::fs::write(&config, b"original").unwrap();
        assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
        assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 1);
        assert_eq!(std::fs::read(&config).unwrap(), b"original");
        std::fs::remove_dir_all(base).unwrap();
    }
}
