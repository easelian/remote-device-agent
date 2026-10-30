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
#[path = "posix/mod.rs"]
mod platform;

#[cfg(unix)]
pub use platform::ProfileLocks;

#[cfg(all(windows, feature = "experimental-windows-private-storage"))]
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
