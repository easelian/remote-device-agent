//! Native service entry points always require an already-paired identity.
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-startup")
))]
use crate::cli::{Action, LaunchArgs, parse};
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-startup")
))]
use crate::error::Error;
use crate::error::Result;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-startup")
))]
use std::path::{Path, PathBuf};

#[cfg(all(windows, feature = "experimental-windows-startup"))]
mod windows;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
mod posix;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-startup")
))]
pub(super) fn arguments(
    args: &[String],
    server: Option<&str>,
) -> Result<(String, LaunchArgs, Option<PathBuf>)> {
    let (name, remaining) = match args {
        [flag, name, remaining @ ..]
            if flag == "--service-name" && crate::startup::valid_name(name) =>
        {
            (name, remaining)
        }
        _ => {
            return Err(Error::message(
                "Use service-run --service-name NAME --config ABSOLUTE_PATH --server URL",
            ));
        }
    };
    let mut launch_args = Vec::new();
    let mut state_directory = None;
    let mut items = remaining.iter();
    while let Some(argument) = items.next() {
        if argument == "--state-directory" {
            let directory = items.next().filter(|value| Path::new(value).is_absolute() && !value.chars().any(char::is_control))
                .ok_or_else(|| Error::message("--state-directory requires an absolute Unicode path without control characters"))?;
            if state_directory.replace(PathBuf::from(directory)).is_some() {
                return Err(Error::message("Duplicate service state directory"));
            }
        } else {
            launch_args.push(argument.clone());
        }
    }
    let Action::Launch(mut launch) = parse(&launch_args, server)? else {
        return Err(Error::message("A service requires Agent launch arguments"));
    };
    if launch.pair_only
        || launch.name.is_some()
        || !launch.roots.is_empty()
        || launch
            .config
            .as_deref()
            .is_none_or(|path| !Path::new(path).is_absolute() || path.chars().any(char::is_control))
    {
        return Err(Error::message(
            "A service requires an absolute existing --config and cannot change pairing",
        ));
    }
    launch.no_browser = true;
    Ok((name.clone(), launch, state_directory))
}

pub fn run(args: &[String], server: Option<&str>) -> Result<()> {
    if args == ["--help"] {
        println!(
            "rdh service-run --service-name NAME --config ABSOLUTE_PATH --server URL [--recover-lock] [--state-directory ABSOLUTE_PATH]\n\nNative service entry point for an already-paired Agent. Windows requires the SCM; POSIX receives the service manager's stop signals.\n--state-directory keeps service audit state beneath the specified private directory."
        );
        return Ok(());
    }
    #[cfg(all(windows, feature = "experimental-windows-startup"))]
    {
        windows::run(args, server)
    }
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-install")
    ))]
    {
        posix::run(args, server)
    }
    #[cfg(not(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-install"),
        all(windows, feature = "experimental-windows-startup")
    )))]
    {
        let _ = (args, server);
        Err(crate::error::Error::message(
            "Native service hosting is unavailable in this build",
        ))
    }
}
