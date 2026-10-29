//! The Windows SCM runs the Agent itself, without an external service wrapper.
use crate::error::Result;

#[cfg(all(windows, feature = "experimental-windows-startup"))]
#[path = "service_host_windows.rs"]
mod windows;

pub fn run(args: &[String], server: Option<&str>) -> Result<()> {
    if args == ["--help"] {
        println!(
            "rdh service-run --service-name NAME --config ABSOLUTE_PATH --server URL [--recover-lock] [--state-directory ABSOLUTE_PATH]\n\nWindows SCM entry point for an already-paired Agent. Register this command as an own-process service; it does not register or start a service from a terminal.\n--state-directory keeps service audit state beneath the specified private directory."
        );
        return Ok(());
    }
    #[cfg(all(windows, feature = "experimental-windows-startup"))]
    {
        windows::run(args, server)
    }
    #[cfg(not(all(windows, feature = "experimental-windows-startup")))]
    {
        let _ = (args, server);
        Err(crate::error::Error::message(
            "Native Windows service hosting is unavailable in this build",
        ))
    }
}
