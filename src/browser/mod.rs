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
    "browser_files",
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
    pub downloads: bool,
    pub uploads: bool,
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
mod files;
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

#[cfg(target_os = "linux")]
pub(crate) fn storage_root(config: &std::path::Path, token: &str) -> Option<PathBuf> {
    config.parent().map(|parent| {
        parent
            .join(".rdh-browser")
            .join(&crate::config::sha256_hex(token.as_bytes())[..24])
    })
}

#[cfg(target_os = "linux")]
fn storage_lock(root: &std::path::Path) -> PathBuf {
    // The lock must survive deletion of the namespace payload. Config files
    // with different names but the same parent/token share this exclusion.
    root.with_extension("agent")
}

/// Called only after Agent shutdown and while the binding's lifecycle lock is held.
/// Retire this binding's browser data without touching other namespaces.
pub(crate) fn remove_binding(config: &std::path::Path, token: &str) -> crate::error::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let Some(root) = storage_root(config, token) else {
            return Ok(());
        };
        let parent = root.parent().unwrap();
        match std::fs::symlink_metadata(parent) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        crate::storage::private_metadata(parent, true)?;
        let path = storage_lock(&root);
        crate::profile_lock::ProfileLocks::recover(std::slice::from_ref(&path))?;
        let _lock = crate::profile_lock::ProfileLocks::acquire(&[path])?;
        match crate::storage::private_metadata(&root, true) {
            Err(crate::error::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        process::recover(&root).map_err(|error| crate::error::Error::message(error.to_string()))?;
        std::fs::remove_dir_all(&root)?;
        crate::storage::sync_directory(&std::fs::File::open(root.parent().unwrap())?)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (config, token);
    Ok(())
}

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
    pub fn release_stopped_storage(&self) {}
    pub fn dispatch(
        &self,
        _: &str,
        _: &Value,
        _: &Principal,
        _: Instant,
        _: Option<&str>,
    ) -> Result<Value> {
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
    #[cfg(target_os = "linux")]
    #[test]
    fn retiring_binding_requires_exclusive_storage_and_preserves_neighbors() {
        let temp = std::env::temp_dir().join(format!(
            "rdh-browser-retire-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        crate::storage::create_private_dir(&temp).unwrap();
        let config = temp.join("device.json");
        let root = storage_root(&config, "retired-token").unwrap();
        let neighbor = storage_root(&config, "other-token").unwrap();
        for path in [
            root.parent().unwrap(),
            &root,
            &neighbor,
            &root.join("profiles"),
            &root.join("artifacts"),
        ] {
            crate::storage::create_private_dir(path).unwrap();
        }
        crate::storage::write_private_replace(
            &root.join("profiles/test"),
            b"synthetic login state",
        )
        .unwrap();
        crate::storage::write_private_replace(&neighbor.join("keep"), b"other binding").unwrap();
        let mut live = crate::profile_lock::ProfileLocks::acquire(&[storage_lock(&root)]).unwrap();
        assert!(remove_binding(&temp.join("alias.json"), "retired-token").is_err());
        assert!(root.join("profiles/test").exists());
        live.release().unwrap();
        remove_binding(&config, "retired-token").unwrap();
        assert!(!root.exists());
        assert_eq!(
            std::fs::read(neighbor.join("keep")).unwrap(),
            b"other binding"
        );
        remove_binding(&config, "retired-token").unwrap();
        // A replaced namespace must never redirect retirement to another directory.
        std::os::unix::fs::symlink(&neighbor, &root).unwrap();
        assert!(remove_binding(&config, "retired-token").is_err());
        assert!(neighbor.join("keep").exists());
        std::fs::remove_dir_all(temp).unwrap();
    }
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
