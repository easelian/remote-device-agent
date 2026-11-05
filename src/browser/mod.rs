//! Opt-in browser sessions. The Hub supplies identity outside model arguments.
#[cfg(not(target_os = "linux"))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(any(test, not(target_os = "linux")))]
use serde_json::json;

pub const OPERATIONS: &[&str] = &[
    "browser_open",
    "browser_sessions",
    "browser_profiles",
    "browser_tabs",
    "browser_observe",
    "browser_screenshot",
    "browser_act",
    "browser_wait",
    "browser_control",
    "browser_human",
];
pub const MUTATIONS: &[&str] = &["browser_open", "browser_act", "browser_control"];

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub persistent_profiles: bool,
    pub executable: Option<PathBuf>,
}

impl Settings {
    pub fn parse(value: Option<&Value>) -> crate::error::Result<Self> {
        let settings: Self = value
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        if settings
            .executable
            .as_ref()
            .is_some_and(|p| !p.is_absolute() || p.as_os_str().is_empty())
        {
            return Err(crate::error::Error::message(
                "browser.executable must be an absolute path",
            ));
        }
        Ok(settings)
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub version: u32,
    pub owner: String,
    pub user: String,
    pub channel: String,
}

impl Principal {
    pub fn valid(&self) -> bool {
        self.version == 1
            && ["ai", "human"].contains(&self.channel.as_str())
            && [&self.owner, &self.user].iter().all(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
    }
}

#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub state: &'static str,
}

impl Failure {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            state: "not_sent",
        }
    }
    fn unknown(code: &'static str) -> Self {
        Self {
            code,
            state: "unknown",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for Failure {}
type Result<T> = std::result::Result<T, Failure>;

#[cfg(target_os = "linux")]
mod cdp;
#[cfg(target_os = "linux")]
mod image;
#[cfg(target_os = "linux")]
mod page;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
mod profiles;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(target_os = "linux")]
pub use runtime::Browsers;

#[cfg(not(target_os = "linux"))]
pub struct Browsers;
#[cfg(not(target_os = "linux"))]
impl Browsers {
    pub fn new(_: &Settings, _: Option<&Path>, _: &str) -> Self {
        Self
    }
    pub fn info(&self) -> Value {
        json!({"version":1,"available":false,"reason":"platform_not_supported"})
    }
    pub fn dispatch(&self, _: &str, _: &Value, _: &Principal, _: Instant) -> Result<Value> {
        Err(Failure::new("browser_unavailable"))
    }
    pub fn stop_all(&self) {}
    pub fn pause_all(&self) {}
    pub fn mark_online(&self) {}
    pub fn expire_offline(self: &std::sync::Arc<Self>) {}
    pub async fn close(&self) -> bool {
        true
    }
}

// Keep all protocol errors content-free: a page can put secrets in CDP errors.
#[cfg(target_os = "linux")]
fn io_error(_: impl std::fmt::Display) -> Failure {
    Failure::new("browser_io_error")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_are_explicit_and_principals_are_not_free_form() {
        assert!(!Settings::parse(None).unwrap().enabled);
        assert!(Settings::parse(Some(&json!({"executable":"relative"}))).is_err());
        assert!(
            Settings::parse(Some(&json!({"enabled":true,"arguments":["--no-sandbox"]}))).is_err()
        );
        assert!(
            !Principal {
                version: 1,
                owner: "caller".into(),
                user: "a".repeat(64),
                channel: "ai".into()
            }
            .valid()
        );
        assert!(
            Principal {
                version: 1,
                owner: "a".repeat(64),
                user: "a".repeat(64),
                channel: "ai".into()
            }
            .valid()
        );
    }
}
