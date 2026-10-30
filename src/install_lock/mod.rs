//! Native installation exclusion and crash markers.
//! A live Agent holds a shared lock; an offline updater must hold it exclusively.

#[cfg(all(windows, feature = "experimental-windows-install"))]
mod windows;

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub use windows::{ManagerLock, PENDING, RunningInstall, current_installation, pointer};

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::{
    MAX_PENDING_BYTES, Object, command_target, pending, pointer_path, private_object,
    record_profile, remove_same, set_pointer, version_binary,
};

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
mod posix;

#[cfg(target_os = "linux")]
pub use posix::{ControllerLock, LaunchBarrier};
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub use posix::{
    ManagerLock, PENDING, RunningInstall, RunningRecord, current_installation, pointer,
};
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub(crate) use posix::{pending, record_profile, version_binary};

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
pub struct RunningInstall;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
impl RunningInstall {
    pub fn acquire_current() -> crate::error::Result<Option<Self>> {
        Ok(None)
    }
    pub fn acquire_launch_current() -> crate::error::Result<Option<Self>> {
        Ok(None)
    }
    pub fn check_launch_profile(&self, _: &std::path::Path, _: bool) -> crate::error::Result<()> {
        Ok(())
    }
    pub fn finish_launch(&mut self) {}
    pub fn mark_running(&mut self, _: &std::path::Path, _: bool) -> crate::error::Result<()> {
        Ok(())
    }
    pub fn mark_clean(&mut self) -> crate::error::Result<()> {
        Ok(())
    }
    pub fn register_profile(&self, _: &std::path::Path) -> crate::error::Result<()> {
        Ok(())
    }
}
