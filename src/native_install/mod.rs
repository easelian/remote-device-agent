//! Recoverable, offline native version-pointer transactions on Linux and macOS.
//! The caller must hold the exclusive installation manager lock.

#[cfg(all(windows, feature = "experimental-windows-install"))]
mod windows;
#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::check_executable;
#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::install_for_locked_profile;
#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::pin_stored;
#[cfg(all(windows, feature = "experimental-windows-install"))]
pub use windows::{
    install, pending_record, previous, recover, stage, switch, validate_profiles, verify_stored,
};

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
mod posix;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub(crate) use posix::check_executable;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub(crate) use posix::install_for_locked_profile;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub use posix::{
    install, pending_record, previous, recover, stage, switch, validate_profiles, verify_stored,
};
#[cfg(target_os = "linux")]
pub use posix::{preflight_service, restore_service, verify_service_protocol};

#[cfg(all(test, target_os = "macos", not(feature = "experimental-macos-install")))]
mod macos_default_tests {
    #[test]
    fn installation_is_unavailable_without_the_experimental_feature() {
        let prefix = std::env::temp_dir().join(format!(
            "rdh-unavailable-install-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(
            crate::upgrade_cli::install(&["--prefix".into(), prefix.to_str().unwrap().into(),])
                .is_err()
        );
        assert!(!prefix.exists());
    }
}

#[cfg(all(test, windows, not(feature = "experimental-windows-install")))]
mod windows_default_tests {
    #[test]
    fn installation_is_unavailable_without_the_experimental_feature() {
        let prefix = std::env::temp_dir().join(format!(
            "rdh-unavailable-install-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(
            crate::upgrade_cli::install(&["--prefix".into(), prefix.to_str().unwrap().into()])
                .is_err()
        );
        assert!(!prefix.exists());
    }
}
