//! Stable command names for versioned native installations.

pub const COMMAND_NAMES: [&str; 3] = ["rdh", "remote-device-agent", "remote-device-hub"];

#[cfg(all(windows, feature = "experimental-windows-install"))]
mod windows {
    use std::{fs, io, path::Path, process::Command};

    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, GetConsoleCP, SetConsoleCtrlHandler,
    };

    use crate::error::{Error, Result};

    fn exists(path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    // A handler belongs only to this process. Do not set the inheritable
    // "ignore Ctrl+C" flag: the selected Agent must receive console signals.
    unsafe extern "system" fn console_signal(event: u32) -> i32 {
        i32::from(matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT))
    }

    struct ConsoleHandler(bool);
    impl ConsoleHandler {
        fn install() -> Result<Self> {
            let attached = unsafe { GetConsoleCP() } != 0;
            if attached && unsafe { SetConsoleCtrlHandler(Some(console_signal), 1) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
            Ok(Self(attached))
        }
    }
    impl Drop for ConsoleHandler {
        fn drop(&mut self) {
            if self.0 {
                unsafe { SetConsoleCtrlHandler(Some(console_signal), 0) };
            }
        }
    }

    fn run<T>(binary: &Path, _pinned_program: T) -> Result<Option<i32>> {
        let _console = ConsoleHandler::install()?;
        // No installation lock crosses spawn. In particular, an "upgrade"
        // child must be able to acquire its exclusive lock immediately. The
        // immutable selected program and its ancestors remain pinned, and the
        // child validates the current installation before any managed action.
        let status = Command::new(binary)
            .args(std::env::args_os().skip(1))
            .status()?;
        Ok(Some(status.code().ok_or_else(|| {
            Error::message("Selected native command returned no exit code")
        })?))
    }

    pub fn forward() -> Result<Option<i32>> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let Some(name) = executable.file_stem().and_then(|s| s.to_str()) else {
            return Ok(None);
        };
        if !super::COMMAND_NAMES
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known))
        {
            return Ok(None);
        }
        let Some(prefix) = executable.parent() else {
            return Ok(None);
        };
        let user =
            exists(&prefix.join(".native-install.json"))? || exists(&prefix.join("current.json"))?;
        let service = exists(&prefix.join(".service-install.json"))?;
        if user && service {
            return Err(Error::message("Conflicting native installation records"));
        }
        if user {
            let (binary, pinned) = crate::install_lock::windows::command_target(prefix)?;
            return run(&binary, pinned);
        }
        if service {
            let (binary, pinned) = crate::service_install::windows::update::command_target(prefix)?;
            return run(&binary, pinned);
        }
        // Version binaries and downloaded standalone executables have no
        // installation record beside them and execute normally.
        Ok(None)
    }
}

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub use windows::forward;
