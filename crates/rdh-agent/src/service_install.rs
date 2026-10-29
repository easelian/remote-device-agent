//! Local service installation is separate from remote startup authorization.
use crate::error::Result;

#[cfg(all(windows, feature = "experimental-windows-install"))]
#[path = "service_install_windows.rs"]
pub(crate) mod windows;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
#[path = "service_install_posix.rs"]
pub(crate) mod posix;

pub fn run(args: &[String]) -> Result<()> {
    if matches!(args, [help] if help == "--help" || help == "-h") {
        #[cfg(windows)]
        println!(
            "rdh install-service --prefix ABSOLUTE_DIRECTORY --data ABSOLUTE_DIRECTORY --config ABSOLUTE_PATH --name NAME\n\nCreates a new Windows LocalService installation from a stopped, paired, unbound profile.\nProgram and data directories must be absent and have existing protected parents.\nThe source profile and its policy are copied without changing device identity.\nThe service is registered for manual start and is not started or enabled at boot.\nThe source profile is retained; do not run both copies of the same device identity."
        );
        #[cfg(not(windows))]
        println!(
            "rdh install-service --prefix ABSOLUTE_DIRECTORY --config ABSOLUTE_PATH --state-directory ABSOLUTE_DIRECTORY --service-user USER --name NAME [--migrate-from LEGACY_PREFIX]\nrdh install-service --recover-migration|--undo-migration --prefix NATIVE_INSTALL\n\nRun as root with an existing non-root service account and its paired, stopped, unbound private configuration and state directory.\nProgram files are root-owned; credentials, policy, audit and startup state stay with the service user.\nCreates a disabled systemd service or LaunchDaemon; start it locally and verify its Hub connection.\nUse an existing protected parent for the new program directory. Interrupted installation files are retained for inspection.\n--migrate-from adopts a stopped, bound legacy system service created by the POSIX installer. It keeps the old program, profile, pending approvals and boot setting. Stop the systemd unit or boot out the LaunchDaemon first.\nMigration recovery completes the recorded operation; undo restores the exact old definition and blocks native startup. These commands never start or stop a service."
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
