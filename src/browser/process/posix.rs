//! Platform process ownership behind the common browser launcher.
#[cfg(target_os = "linux")]
pub(super) mod linux;
#[cfg(target_os = "linux")]
mod supervisor;
#[cfg(target_os = "linux")]
pub(crate) use supervisor::run_helper;
#[cfg(target_os = "linux")]
pub use supervisor::{Child, retired};

#[cfg(target_os = "macos")]
mod direct;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use direct::{Child, retired};
