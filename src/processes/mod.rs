//! Shell jobs with one owner for each native process and its cleanup.
//!
//! POSIX process groups do not confine commands which create a new session/group.
//! Windows children enter an owned Job before they run. Directory handles retain
//! validated working directories; they do not reduce the shell's OS authority.
//! File roots are not a shell sandbox.
use crate::{config::DeviceConfig, files::Files, sandbox::Sandbox};
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

#[cfg(any(
    all(test, unix),
    all(target_os = "macos", feature = "experimental-macos-processes")
))]
mod macos;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
))]
pub const PROCESS_OPERATIONS: &[&str] = &[
    "start_process",
    "read_process",
    "list_processes",
    "write_process_stdin",
    "terminate_process",
];
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
)))]
pub const PROCESS_OPERATIONS: &[&str] = &[];

pub type ExitHook = Arc<dyn Fn(String, Value) + Send + Sync>;

#[derive(Debug)]
pub struct ProcessFailure {
    pub code: &'static str,
    pub message: String,
    pub state: &'static str,
    pub summary: Value,
}

impl ProcessFailure {
    fn new(code: &'static str, message: impl Into<String>, state: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            state,
            summary: serde_json::json!({}),
        }
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
))]
pub use native::Processes;

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
)))]
pub struct Processes;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
)))]
impl Processes {
    pub(crate) const TREE_TERMINATION: &str = "unavailable";

    pub fn new(
        _: &DeviceConfig,
        _: Arc<Files>,
        _: Vec<String>,
        _: Option<ExitHook>,
    ) -> Result<Self, String> {
        Ok(Self)
    }
    pub fn with_sandbox(
        _: &DeviceConfig,
        _: Arc<Files>,
        _: Vec<String>,
        _: Option<ExitHook>,
        _: Arc<Sandbox>,
    ) -> Result<Self, String> {
        Ok(Self)
    }
    pub fn dispatch(
        &self,
        _: &str,
        _: &Value,
        _: &str,
        _: Instant,
        _: &AtomicBool,
    ) -> Result<Value, ProcessFailure> {
        Err(ProcessFailure::new(
            "unsupported",
            "Native process supervision is unsupported on this platform",
            "not_sent",
        ))
    }
    pub fn mark_offline(&self) {}
    pub fn mark_online(&self) {}
    pub fn stop_offline_jobs(&self) {}
    pub fn quiescent(&self) -> bool {
        true
    }
    pub fn close(&self, _: Duration) -> bool {
        true
    }
}

#[cfg(all(
    test,
    target_os = "macos",
    not(feature = "experimental-macos-processes")
))]
mod macos_default_tests {
    use super::*;

    #[test]
    fn process_operations_are_disabled_without_native_validation_feature() {
        assert!(PROCESS_OPERATIONS.is_empty());
        let error = Processes
            .dispatch(
                "start_process",
                &serde_json::json!({"root":"work","command":"touch forbidden"}),
                "default-gate",
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(error.code, "unsupported");
        assert_eq!(error.state, "not_sent");
    }
}

#[cfg(all(test, windows, not(feature = "experimental-windows-processes")))]
mod windows_default_tests {
    use super::*;

    #[test]
    fn process_operations_remain_disabled_without_native_process_feature() {
        assert!(PROCESS_OPERATIONS.is_empty());
        let error = Processes
            .dispatch(
                "start_process",
                &serde_json::json!({
                    "root":"work", "command":"echo forbidden"
                }),
                "default-gate",
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(error.code, "unsupported");
        assert_eq!(error.state, "not_sent");
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
))]
mod native;

#[cfg(all(windows, feature = "experimental-windows-startup"))]
pub(crate) use native::platform::quote as windows_argument;

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use native::platform::check_program as check_native_program;
