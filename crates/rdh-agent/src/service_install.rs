//! Local service installation is separate from remote startup authorization.
use crate::error::Result;

#[cfg(all(windows, feature = "experimental-windows-install"))]
#[path = "service_install_windows.rs"]
mod windows;

pub fn run(args: &[String]) -> Result<()> {
    if matches!(args, [help] if help == "--help" || help == "-h") {
        println!(
            "rdh install-service --prefix ABSOLUTE_DIRECTORY --data ABSOLUTE_DIRECTORY --config ABSOLUTE_PATH --name NAME\n\nCreates a new Windows LocalService installation from a stopped, paired, unbound profile.\nProgram and data directories must be absent and have existing protected parents.\nThe source profile and its policy are copied without changing device identity.\nThe service is registered for manual start and is not started or enabled at boot.\nThe source profile is retained; do not run both copies of the same device identity."
        );
        return Ok(());
    }
    #[cfg(all(windows, feature = "experimental-windows-install"))]
    {
        windows::run(args)
    }
    #[cfg(not(all(windows, feature = "experimental-windows-install")))]
    {
        let _ = args;
        Err(crate::error::Error::message(
            "Native LocalService installation requires the experimental Windows installation feature",
        ))
    }
}
