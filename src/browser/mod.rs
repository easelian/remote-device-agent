//! Opt-in browser sessions. The Hub supplies identity outside model arguments.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
)))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
)))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(any(
    test,
    not(any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    ))
))]
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

/// Internal process entry, recognized before public CLI arguments are parsed.
#[doc(hidden)]
pub fn run_internal_supervisor() -> Option<crate::error::Result<()>> {
    #[cfg(all(target_os = "linux", feature = "experimental-browser-bidi"))]
    if let Some(result) = process::firefox::run_helper() {
        return Some(result.map_err(crate::error::Error::from));
    }
    #[cfg(all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    ))]
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        process::run_supervisor(&args).map(|result| result.map_err(crate::error::Error::from))
    }
    #[cfg(not(all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )))]
    None
}
pub const MUTATIONS: &[&str] = &["browser_open", "browser_act", "browser_control"];

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub capability_v2: bool,
    pub firefox_executable: Option<PathBuf>,
    pub enabled: bool,
    pub persistent_profiles: bool,
    pub downloads: bool,
    pub uploads: bool,
    pub executable: Option<PathBuf>,
    pub sites: sites::Sites,
    pub shell_coexistence: Coexistence,
}

/// Whether browser sessions are offered on a device that also gives the AI a
/// shell. Text on a web page can mislead the AI into running a command, and a
/// command outside a sandbox can read the browser's stored logins and change
/// the local policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coexistence {
    /// The owner accepts both together.
    #[default]
    Allow,
    /// Only while shell commands run inside the sandbox.
    SandboxedOnly,
    /// Never together: the shell stays, browser sessions are not offered.
    Deny,
}

/// The shell that the device gives the AI, as far as browser sessions care.
#[derive(Clone, Copy, Debug, Default)]
pub struct Shell {
    pub enabled: bool,
    /// Its commands run inside the sandbox.
    pub sandboxed: bool,
}

impl Coexistence {
    /// Why browser sessions are not offered beside this shell, when they are not.
    pub fn refuses(self, shell: Shell) -> Option<&'static str> {
        match self {
            _ if !shell.enabled => None,
            Self::Allow => None,
            Self::SandboxedOnly if shell.sandboxed => None,
            Self::SandboxedOnly => Some("shell_not_sandboxed"),
            Self::Deny => Some("shell_enabled"),
        }
    }
}

mod selection;

impl Settings {
    pub fn parse(value: Option<&Value>) -> crate::error::Result<Self> {
        let settings: Self = value
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        if [&settings.executable, &settings.firefox_executable]
            .into_iter()
            .flatten()
            .any(|p| !p.is_absolute() || p.as_os_str().is_empty())
        {
            return Err(crate::error::Error::message(
                "browser executable paths must be absolute",
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

// The policy is read on every platform. Only a platform that runs browser
// sessions applies it.
#[cfg_attr(
    not(any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    )),
    allow(dead_code)
)]
pub mod sites;

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod control;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod engine;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod files;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod image;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod page;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod process;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod profiles;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
mod runtime;
#[cfg(all(
    feature = "experimental-browser-bidi",
    any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    )
))]
mod semantic;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
pub use runtime::Browsers;

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
pub(crate) fn storage_root(config: &std::path::Path, token: &str) -> Option<PathBuf> {
    config.parent().map(|parent| {
        parent
            .join(".rdh-browser")
            .join(&crate::config::sha256_hex(token.as_bytes())[..24])
    })
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
fn storage_lock(root: &std::path::Path) -> PathBuf {
    // The lock must survive deletion of the namespace payload. Config files
    // with different names but the same parent/token share this exclusion.
    root.with_extension("agent")
}

/// Called only after Agent shutdown and while the binding's lifecycle lock is held.
/// Retire this binding's browser data without touching other namespaces.
pub(crate) fn remove_binding(config: &std::path::Path, token: &str) -> crate::error::Result<()> {
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    ))]
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
        #[cfg(unix)]
        crate::storage::sync_directory(&std::fs::File::open(root.parent().unwrap())?)?;
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    )))]
    let _ = (config, token);
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
)))]
pub struct Browsers;
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
)))]
impl Browsers {
    pub fn new(_: &Settings, _: Option<&Path>, _: &str, _: Shell) -> Self {
        Self
    }
    pub fn info(&self) -> Value {
        json!({"version":1,"available":false,"reason":"platform_not_supported"})
    }
    pub fn release_stopped_storage(&self) {}
    pub fn resolve_with(&self, _: sites::Resolver) {}
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
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
fn io_error(_: impl std::fmt::Display) -> Failure {
    Failure::new("browser_io_error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_decides_whether_a_shell_and_browser_sessions_go_together() {
        let none = Shell::default();
        let plain = Shell {
            enabled: true,
            sandboxed: false,
        };
        let sandboxed = Shell {
            enabled: true,
            sandboxed: true,
        };
        // Without a shell there is nothing to decide.
        for coexistence in [
            Coexistence::Allow,
            Coexistence::SandboxedOnly,
            Coexistence::Deny,
        ] {
            assert_eq!(coexistence.refuses(none), None);
        }
        assert_eq!(Coexistence::Allow.refuses(plain), None);
        assert_eq!(Coexistence::Allow.refuses(sandboxed), None);
        assert_eq!(
            Coexistence::SandboxedOnly.refuses(plain),
            Some("shell_not_sandboxed")
        );
        assert_eq!(Coexistence::SandboxedOnly.refuses(sandboxed), None);
        assert_eq!(Coexistence::Deny.refuses(plain), Some("shell_enabled"));
        assert_eq!(Coexistence::Deny.refuses(sandboxed), Some("shell_enabled"));

        // The policy says nothing: both together are accepted, as before.
        let parse = |value: Value| Settings::parse(Some(&value));
        assert_eq!(
            parse(serde_json::json!({"enabled":true}))
                .unwrap()
                .shell_coexistence,
            Coexistence::Allow
        );
        for (written, expected) in [
            ("allow", Coexistence::Allow),
            ("sandboxed_only", Coexistence::SandboxedOnly),
            ("deny", Coexistence::Deny),
        ] {
            let settings = parse(serde_json::json!({"shell_coexistence":written})).unwrap();
            assert_eq!(settings.shell_coexistence, expected);
        }
        assert!(parse(serde_json::json!({"shell_coexistence":"sandbox"})).is_err());
        assert!(parse(serde_json::json!({"shell_coexistence":true})).is_err());
    }
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    ))]
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
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&neighbor, &root).unwrap();
            assert!(remove_binding(&config, "retired-token").is_err());
            assert!(neighbor.join("keep").exists());
        }
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
