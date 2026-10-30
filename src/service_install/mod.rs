//! Local service installation is separate from remote startup authorization.
use crate::error::Result;

#[cfg(all(windows, feature = "experimental-windows-install"))]
mod windows;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
mod posix;

// Select the existing platform guard without wrapping its lease. Callers must
// retain the returned owner throughout launch, operation cleanup and shutdown.
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub(crate) use posix::{Lease, check_service_entry, runtime_guard};
#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::update::{Lease, command_target, runtime_guard};

// Builds without native service installation previously skipped this guard.
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
pub(crate) fn runtime_guard(
    _: &crate::cli::LaunchArgs,
    _: Option<&std::path::Path>,
) -> Result<Option<()>> {
    Ok(None)
}

pub fn run(args: &[String]) -> Result<()> {
    if matches!(args, [help] if help == "--help" || help == "-h") {
        #[cfg(windows)]
        println!(
            "rdh install-service --prefix ABSOLUTE_DIRECTORY --data ABSOLUTE_DIRECTORY --config ABSOLUTE_PATH --name NAME\n\nCreates a new Windows LocalService installation from a stopped, paired, unbound profile.\nProgram and data directories must be absent and have protected parents.\nThe source profile and policy are copied. The source remains; do not run both copies of the same device identity.\nThe service is registered for manual start. Start it locally and verify its Hub connection."
        );
        #[cfg(not(windows))]
        println!(
            "rdh install-service --prefix ABSOLUTE_DIRECTORY --config ABSOLUTE_PATH --state-directory ABSOLUTE_DIRECTORY --service-user USER --name NAME\n\nRun as root with an existing non-root service account and its paired, stopped, unbound private configuration and state directory.\nProgram files are root-owned; configuration, policy, audit and startup state stay with the service user.\nCreates a disabled systemd service or LaunchDaemon. Start it locally and verify its Hub connection.\nUse an existing protected parent for the new program directory. Interrupted installation files are retained for inspection."
        );
        return Ok(());
    }
    #[cfg(all(windows, feature = "experimental-windows-install"))]
    {
        windows::run(args)
    }
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-install")
    ))]
    {
        posix::run(args)
    }
    #[cfg(not(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-install"),
        all(windows, feature = "experimental-windows-install")
    )))]
    {
        let _ = args;
        Err(crate::error::Error::message(
            "Native service installation is unavailable in this build",
        ))
    }
}
