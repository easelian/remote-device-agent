//! Opt-in browser sessions. The Hub supplies identity outside model arguments.
#[cfg(not(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
)))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
)))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(any(
    test,
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    ))
))]
use serde_json::json;

/// Everything a peer may ask of the browser. A session is opened, driven and
/// closed through `browser_do`; no other name reaches a session.
pub const OPERATIONS: &[&str] = &[
    "browser_files",
    "browser_human",
    "browser_do",
    "browser_read",
    "browser_see",
];

/// Internal process entry, recognized before public CLI arguments are parsed.
#[doc(hidden)]
pub fn run_internal_supervisor() -> Option<crate::error::Result<()>> {
    #[cfg(target_os = "linux")]
    if let Some(result) = process::run_posix_supervisor() {
        return Some(result.map_err(crate::error::Error::from));
    }
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
pub const MUTATIONS: &[&str] = &["browser_do"];

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Whether the session runtime may be used. On unless the policy says
    /// `false`; `enabled` still decides whether there is a browser at all.
    pub capability_v2: bool,
    pub firefox_executable: Option<PathBuf>,
    pub enabled: bool,
    pub persistent_profiles: bool,
    pub downloads: bool,
    pub uploads: bool,
    pub executable: Option<PathBuf>,
    pub sites: sites::Sites,
    /// Where the browser sends what it asks for. Only this file says so:
    /// a proxy sees everything the browser loads.
    pub proxy: Option<Proxy>,
    pub shell_coexistence: Coexistence,
}

/// `browser.proxy` in the local policy.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Proxy {
    /// `http://host:port` or `socks5://host:port`, without a user or password.
    pub server: String,
    /// Hosts that are reached directly: a name, `.suffix`, an address or a
    /// range. This device itself is always reached directly.
    #[serde(default)]
    pub bypass: Vec<String>,
}

impl Proxy {
    fn check(&self) -> std::result::Result<(), &'static str> {
        const SERVER: &str =
            "browser.proxy.server must be http://host:port or socks5://host:port, without a user";
        let url = url::Url::parse(&self.server).map_err(|_| SERVER)?;
        if !matches!(url.scheme(), "http" | "socks5")
            || url.host_str().is_none_or(str::is_empty)
            || url.port_or_known_default().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
            || self.server.len() > 512
        {
            return Err(SERVER);
        }
        if self.bypass.len() > 64
            || self.bypass.iter().any(|entry| {
                entry.is_empty()
                    || entry.len() > 255
                    || !entry
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".:/*-_[]".contains(&b))
            })
        {
            return Err(
                "browser.proxy.bypass takes up to 64 host names, .suffixes, addresses or ranges",
            );
        }
        Ok(())
    }
    /// What the browser is started with to use the proxy.
    pub fn arguments(&self) -> Vec<String> {
        let mut arguments = vec![format!(
            "--proxy-server={}",
            self.server.trim_end_matches('/')
        )];
        if !self.bypass.is_empty() {
            arguments.push(format!("--proxy-bypass-list={}", self.bypass.join(";")));
        }
        arguments
    }
    /// Whether a host name is on the list of those reached directly.
    pub fn bypasses(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.bypass.iter().any(|entry| {
            let entry = entry.to_ascii_lowercase();
            match entry.strip_prefix("*.").or_else(|| entry.strip_prefix('.')) {
                Some(under) => host == under || host.ends_with(&format!(".{under}")),
                None => host == entry,
            }
        })
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            capability_v2: true,
            firefox_executable: None,
            enabled: false,
            persistent_profiles: false,
            downloads: false,
            uploads: false,
            executable: None,
            sites: sites::Sites::default(),
            proxy: None,
            shell_coexistence: Coexistence::default(),
        }
    }
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

#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
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
        if let Some(proxy) = &settings.proxy {
            proxy.check().map_err(crate::error::Error::message)?;
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
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
mod address;
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
// Sessions run only where the v2 runtime is built. The parts below it are
// still compiled for every platform that can hold browser storage, so that
// they keep building there; without the runtime most of them have no caller.
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
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
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
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
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
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
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
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
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
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
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code, unused_imports)
)]
mod profiles;
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
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
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
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

#[cfg(not(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
)))]
pub struct Browsers;
#[cfg(not(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
)))]
impl Browsers {
    pub fn new(_: &Settings, _: Option<&Path>, _: &str, _: Shell) -> Self {
        Self
    }
    pub fn info(&self) -> Value {
        // Browser storage can exist on some of these platforms; sessions cannot.
        let reason = if cfg!(any(
            target_os = "linux",
            target_os = "macos",
            all(
                windows,
                feature = "experimental-windows-private-storage",
                feature = "experimental-windows-processes"
            )
        )) {
            "platform_or_build_unsupported"
        } else {
            "platform_not_supported"
        };
        json!({"version":1,"available":false,"reason":reason})
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
        // The session runtime is on wherever the browser is, unless it is turned off.
        assert!(Settings::parse(None).unwrap().capability_v2);
        let enabled = Settings::parse(Some(&json!({"enabled":true}))).unwrap();
        assert!(enabled.enabled && enabled.capability_v2);
        let off = Settings::parse(Some(&json!({"enabled":true,"capability_v2":false}))).unwrap();
        assert!(off.enabled && !off.capability_v2);
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

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use serde_json::json;

    fn parsed(proxy: Value) -> crate::error::Result<Settings> {
        Settings::parse(Some(&json!({"proxy":proxy})))
    }

    #[test]
    fn a_proxy_is_an_address_without_a_user_and_a_list_of_hosts_reached_directly() {
        assert!(Settings::parse(None).unwrap().proxy.is_none());
        for server in ["http://10.0.0.1:3128", "http://proxy.corp.example:8080/", "socks5://127.0.0.1:1080"] {
            assert!(parsed(json!({"server":server})).is_ok(), "{server}");
        }
        for server in [
            "",
            "proxy:3128",
            "https://proxy:3128",
            "socks5://proxy",
            "http://user:secret@proxy:3128",
            "http://user@proxy:3128",
            "http://proxy:3128/path",
            "http://proxy:3128/?x=1",
        ] {
            assert!(parsed(json!({"server":server})).is_err(), "{server}");
        }
        assert!(parsed(json!({"bypass":[".corp.example"]})).is_err());
        assert!(parsed(json!({"server":"http://p:1","user":"u"})).is_err());
        for bypass in [json!([""]), json!(["a b"]), json!(["x;y"]), json!(vec!["a"; 65])] {
            assert!(
                parsed(json!({"server":"http://p:1","bypass":bypass})).is_err(),
                "{bypass}"
            );
        }
        let proxy = parsed(json!({"server":"http://10.0.0.1:3128/",
            "bypass":[".corp.example","*.lab.example","intranet","10.0.0.0/8"]}))
        .unwrap()
        .proxy
        .unwrap();
        assert_eq!(
            proxy.arguments(),
            [
                "--proxy-server=http://10.0.0.1:3128",
                "--proxy-bypass-list=.corp.example;*.lab.example;intranet;10.0.0.0/8"
            ]
        );
        for host in ["corp.example", "wiki.Corp.Example.", "a.lab.example", "intranet"] {
            assert!(proxy.bypasses(host), "{host}");
        }
        for host in ["example", "notcorp.example", "intranet.example", "shop.example"] {
            assert!(!proxy.bypasses(host), "{host}");
        }
    }
}
