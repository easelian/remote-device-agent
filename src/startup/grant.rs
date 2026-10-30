//! Administrator-owned, per-config startup grants; never trusted from the user profile.
use std::path::Path;

use crate::error::{Error, Result};
use crate::startup::Mode;
#[cfg(any(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-startup"),
        all(windows, feature = "experimental-windows-startup")
    )
))]
use serde_json::Value;

#[cfg(any(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-startup"),
        all(windows, feature = "experimental-windows-startup")
    )
))]
pub(crate) fn permits(raw: &[u8], config: &Path, mode: Mode) -> Result<bool> {
    if raw.len() > 4096 {
        return Err(Error::message("Managed startup grant is too large"));
    }
    let value: Value = serde_json::from_slice(raw)?;
    Ok(value.get("version").and_then(Value::as_u64) == Some(1)
        && value.get("config").and_then(Value::as_str) == config.to_str()
        && value
            .get("modes")
            .and_then(Value::as_array)
            .is_some_and(|modes| {
                modes.iter().any(|item| {
                    item.as_str()
                        == Some(match mode {
                            Mode::None => "none",
                            Mode::User => "user",
                            Mode::Service => "service",
                        })
                })
            }))
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup")
))]
#[path = "posix/grant.rs"]
mod posix;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup")
))]
pub(crate) use posix::{managed_grant, set_grant};
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub(crate) fn managed_grant(_: &Path, _: Mode) -> Result<bool> {
    Err(Error::message(
        "Managed startup grants are unsupported on this platform",
    ))
}
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub(crate) fn set_grant(_: &Path, _: Mode) -> Result<()> {
    Err(Error::message(
        "Managed startup grants are unsupported on this platform",
    ))
}

#[cfg(all(windows, feature = "experimental-windows-startup"))]
pub(crate) use crate::storage::windows::startup_grants::{managed_grant, set_grant};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grants_are_bound_to_the_exact_config_and_mode() {
        let raw = br#"{"version":1,"config":"/tmp/config.json","modes":["user"]}"#;
        assert!(permits(raw, Path::new("/tmp/config.json"), Mode::User).unwrap());
        assert!(!permits(raw, Path::new("/tmp/config.json"), Mode::Service).unwrap());
        assert!(!permits(raw, Path::new("/tmp/other.json"), Mode::User).unwrap());
        assert!(permits(&[b' '; 4097], Path::new("/tmp/config.json"), Mode::User).is_err());
    }
}
