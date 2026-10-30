//! Platform registration backend. Enabling changes future startup only.

#[cfg(all(target_os = "macos", feature = "experimental-macos-startup"))]
pub(crate) use super::macos::NativeStartup;

#[cfg(target_os = "macos")]
pub(crate) const DEFAULT_SERVICE_NAME: &str = "com.be-one-click.rdh";
#[cfg(windows)]
pub(crate) const DEFAULT_SERVICE_NAME: &str = "RemoteDeviceHub";
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) const DEFAULT_SERVICE_NAME: &str = "rdh.service";

#[cfg(target_os = "linux")]
pub(crate) use super::linux::NativeStartup;

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub(crate) struct NativeStartup;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
impl NativeStartup {
    pub(crate) fn new(_: std::path::PathBuf) -> crate::error::Result<Self> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
}
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
impl crate::startup::StartupBackend for NativeStartup {
    fn enabled(&self, _: &crate::startup::Binding) -> crate::error::Result<bool> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
    fn set(&self, _: &crate::startup::Binding, _: bool) -> crate::error::Result<()> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
    fn create_user(&self) -> crate::error::Result<crate::startup::Binding> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
    fn service_authority(&self) -> bool {
        false
    }
}

#[cfg(all(test, target_os = "macos", not(feature = "experimental-macos-startup")))]
mod macos_default_tests {
    #[test]
    fn startup_requires_explicit_experimental_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-startup-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(super::NativeStartup::new(path.clone()).is_err());
        assert!(crate::startup::STARTUP_OPERATIONS.is_empty());
        assert!(!path.exists());
        assert_eq!(super::DEFAULT_SERVICE_NAME, "com.be-one-click.rdh");
    }
}

#[cfg(all(windows, feature = "experimental-windows-startup"))]
pub(crate) use super::windows::NativeStartup;

#[cfg(all(test, windows, not(feature = "experimental-windows-startup")))]
mod windows_default_tests {
    #[test]
    fn startup_requires_the_native_windows_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-startup-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(super::NativeStartup::new(path.clone()).is_err());
        assert!(crate::startup::STARTUP_OPERATIONS.is_empty());
        assert!(!path.exists());
        assert_eq!(super::DEFAULT_SERVICE_NAME, "RemoteDeviceHub");
    }
}
