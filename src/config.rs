use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use url::{Host, Url};

use crate::deployment_defaults::{MCP_PUBLIC_URL, PUBLIC_URL};
use crate::error::{Error, Result};
use crate::storage;

pub const DEFAULT_SERVER: &str = PUBLIC_URL;

// Deliberately does not implement Debug: this object contains the device token.
#[derive(Clone, Serialize)]
pub struct DeviceConfig {
    pub gateway_url: String,
    pub device_id: String,
    pub token: String,
    pub roots: BTreeMap<String, String>,
    pub allow_shell: bool,
    pub allow_write: bool,
    pub shell: String,
    pub max_processes: u32,
    pub max_process_seconds: u32,
}

pub fn validate_gateway(value: &str) -> Result<String> {
    let invalid = || {
        Error::message("--server must be an HTTPS Hub URL (HTTP is allowed for local debugging)")
    };
    if value != value.trim()
        || value.chars().any(|c| c <= '\u{20}' || c == '\\')
        || value.contains('?')
        || value.contains('#')
    {
        return Err(invalid());
    }
    let url = Url::parse(value).map_err(|_| invalid())?;
    if !url.username().is_empty() || url.password().is_some() || url.path() != "/" {
        return Err(invalid());
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host == "localhost",
        Some(Host::Ipv4(host)) => host == std::net::Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(host)) => host == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}

pub fn same_hub(left: &str, right: &str) -> bool {
    left == right
        || ([PUBLIC_URL, MCP_PUBLIC_URL].contains(&left)
            && [PUBLIC_URL, MCP_PUBLIC_URL].contains(&right))
}

pub fn profile_servers(server: &str) -> Vec<String> {
    if same_hub(server, PUBLIC_URL) {
        let mut servers = vec![PUBLIC_URL.to_owned()];
        if MCP_PUBLIC_URL != PUBLIC_URL {
            servers.push(MCP_PUBLIC_URL.to_owned());
        }
        servers
    } else {
        vec![server.to_owned()]
    }
}

fn valid_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn required_string<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    map.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::message("Invalid device configuration field type"))
}

fn optional_bool(map: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(Error::message("Invalid device configuration parameters")),
    }
}

fn optional_integer(map: &Map<String, Value>, key: &str, default: u32, max: u32) -> Result<u32> {
    let value = match map.get(key) {
        None | Some(Value::Null) => return Ok(default),
        Some(Value::Number(value)) => value.as_f64().filter(|v| v.is_finite() && v.fract() == 0.0),
        _ => None,
    }
    .ok_or_else(|| Error::message("Invalid device configuration parameters"))?;
    if value < 1.0 || value > f64::from(max) {
        return Err(Error::message("Invalid device configuration parameters"));
    }
    Ok(value as u32)
}

#[cfg(not(windows))]
fn default_shell() -> Result<String> {
    Ok("/bin/sh".to_owned())
}

#[cfg(windows)]
fn default_shell() -> Result<String> {
    let root = std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("WINDIR").filter(|value| !value.is_empty()))
        .ok_or_else(|| Error::message("A trusted absolute SystemRoot is required on Windows"))?;
    let root = root
        .to_str()
        .ok_or_else(|| Error::message("SystemRoot is not valid Unicode"))?;
    windows_default_shell(root)
        .ok_or_else(|| Error::message("A trusted absolute SystemRoot is required on Windows"))
}

/// Match the legacy Agent's Windows SystemRoot validation and shell path.
/// This only validates syntax; it does not verify the executable on disk.
#[cfg(any(windows, test))]
fn windows_default_shell(root: &str) -> Option<String> {
    let bytes = root.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
        || root[2..].contains(':')
        || root
            .chars()
            .any(|value| matches!(value, '\0' | '\r' | '\n'))
    {
        return None;
    }
    let components: Vec<_> = root[3..]
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    if components.iter().any(|part| matches!(*part, "." | "..")) {
        return None;
    }
    let mut path = format!("{}:\\", char::from(bytes[0]));
    if !components.is_empty() {
        path.push_str(&components.join("\\"));
        path.push('\\');
    }
    path.push_str("System32\\cmd.exe");
    Some(path)
}

#[cfg(not(windows))]
fn valid_root(value: &str) -> bool {
    Path::new(value).is_absolute()
}

#[cfg(windows)]
fn valid_root(value: &str) -> bool {
    valid_windows_root(value)
}

/// Mirror the legacy Agent's drive and UNC root syntax before any platform
/// file access. This is not a substitute for handle or reparse-point checks.
#[cfg(any(windows, test))]
fn valid_windows_root(value: &str) -> bool {
    if value.encode_utf16().count() > 4096 || value.contains('\0') {
        return false;
    }
    let normalized = value.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return true;
    }
    let Some(share) = normalized.strip_prefix("//") else {
        return false;
    };
    let mut components = share.split('/');
    let (Some(host), Some(name)) = (components.next(), components.next()) else {
        return false;
    };
    [host, name].iter().all(|part| {
        !part.is_empty()
            && !matches!(*part, "." | ".." | "?" | "??")
            && !part.chars().any(|value| {
                value <= '\u{1f}' || matches!(value, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            })
    })
}

impl DeviceConfig {
    pub fn parse(value: Value) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::message("Invalid device configuration"))?;
        const KEYS: &[&str] = &[
            "gateway_url",
            "device_id",
            "token",
            "roots",
            "allow_shell",
            "allow_write",
            "shell",
            "max_processes",
            "max_process_seconds",
        ];
        if let Some(key) = map.keys().find(|key| !KEYS.contains(&key.as_str())) {
            return Err(Error::message(format!(
                "Unknown device configuration field: {key}"
            )));
        }
        let gateway_url = validate_gateway(required_string(map, "gateway_url")?)?;
        let device_id = required_string(map, "device_id")?.to_owned();
        let token = required_string(map, "token")?.to_owned();
        if !valid_name(&device_id) {
            return Err(Error::message("Invalid device ID"));
        }
        if token.encode_utf16().count() < 32 {
            return Err(Error::message("Invalid device token"));
        }
        let entries = map
            .get("roots")
            .and_then(Value::as_object)
            .filter(|roots| !roots.is_empty())
            .ok_or_else(|| Error::message("Invalid device roots"))?;
        let mut roots = BTreeMap::new();
        for (name, value) in entries {
            let root = value
                .as_str()
                .ok_or_else(|| Error::message("Invalid device roots"))?;
            if !valid_name(name)
                || root.encode_utf16().count() > 4096
                || root.contains('\0')
                || !valid_root(root)
            {
                return Err(Error::message("Invalid device roots"));
            }
            roots.insert(name.clone(), root.to_owned());
        }
        let shell = match map.get("shell") {
            None | Some(Value::Null) => default_shell()?,
            Some(Value::String(value)) => value.clone(),
            _ => return Err(Error::message("Invalid device configuration parameters")),
        };
        Ok(Self {
            gateway_url,
            device_id,
            token,
            roots,
            allow_shell: optional_bool(map, "allow_shell", false)?,
            allow_write: optional_bool(map, "allow_write", false)?,
            shell,
            max_processes: optional_integer(map, "max_processes", 4, 16)?,
            max_process_seconds: optional_integer(map, "max_process_seconds", 3600, 86400)?,
        })
    }

    pub fn read(path: &Path) -> Result<Self> {
        let bytes = storage::read_private(path)?;
        Self::parse(serde_json::from_slice(&bytes)?)
    }

    pub fn write_new(&self, path: &Path) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        storage::write_private_new(path, &bytes)
    }
}

pub fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut result = String::with_capacity(64);
    use std::fmt::Write;
    for byte in digest {
        write!(&mut result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

pub fn config_paths(base: &Path, server: &str, profile: &str) -> Result<[PathBuf; 2]> {
    if !valid_name(profile) {
        return Err(Error::message(
            "Profile must contain only letters, numbers, '-' or '_'",
        ));
    }
    let hash = sha256_hex(server.as_bytes());
    let filename = format!("{}-{profile}.json", &hash[..16]);
    Ok([
        base.join("remote-device-hub").join(&filename),
        base.join("remote-device-host").join(filename),
    ])
}

fn path_exists_no_follow(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub struct ProfilePaths {
    pub destination: PathBuf,
    pub lock_targets: Vec<PathBuf>,
}

impl ProfilePaths {
    pub fn resolve(base: &Path, server: &str, profile: &str, custom: Option<&str>) -> Result<Self> {
        if let Some(custom) = custom {
            let destination = absolute_config_path(custom)?;
            return Ok(Self {
                lock_targets: vec![destination.clone()],
                destination,
            });
        }
        let mut all = Vec::new();
        let mut destination = None;
        for origin in profile_servers(server) {
            let paths = config_paths(base, &origin, profile)?;
            if destination.is_none() {
                for path in &paths {
                    if path_exists_no_follow(path)? {
                        destination = Some(path.clone());
                        break;
                    }
                }
            }
            all.extend(paths);
        }
        let destination = destination.unwrap_or_else(|| all[0].clone());
        all.sort();
        all.dedup();
        Ok(Self {
            destination,
            lock_targets: all,
        })
    }
}

pub fn home_directory() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    #[allow(deprecated)]
    std::env::home_dir()
        .ok_or_else(|| Error::message("Could not determine the user's home directory"))
}

pub fn config_base() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    #[cfg(windows)]
    if let Some(value) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    #[cfg(windows)]
    return Ok(home_directory()?.join("AppData").join("Local"));
    #[cfg(not(windows))]
    Ok(home_directory()?.join(".config"))
}

fn absolute_config_path(value: &str) -> Result<PathBuf> {
    let path = if value == "~" {
        home_directory()?
    } else if let Some(rest) = value.strip_prefix("~/") {
        home_directory()?.join(rest)
    } else {
        PathBuf::from(value)
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Convert only a locally canonicalized path to the protocol's root spelling.
/// Windows canonicalize returns an extended path; device configs deliberately
/// reject extended namespaces supplied by a peer or edited configuration.
fn canonical_root_text(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::message("File root is not valid Unicode"))?;
    #[cfg(windows)]
    {
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{unc}"));
        }
        if let Some(drive) = value.strip_prefix(r"\\?\") {
            if valid_windows_root(drive) && drive.as_bytes().get(1) == Some(&b':') {
                return Ok(drive.into());
            }
            return Err(Error::message("File root must be a drive or UNC directory"));
        }
    }
    Ok(value.into())
}

pub fn local_roots(items: &[String]) -> Result<BTreeMap<String, String>> {
    let mut roots = BTreeMap::new();
    if items.is_empty() {
        let cwd = std::env::current_dir()?.canonicalize()?;
        roots.insert("work".to_owned(), canonical_root_text(&cwd)?);
        return Ok(roots);
    }
    for item in items {
        let (name, path) = item
            .split_once('=')
            .filter(|(name, _)| !name.is_empty())
            .ok_or_else(|| Error::message("Each --root must use NAME=PATH"))?;
        if !valid_name(name) {
            return Err(Error::message("Invalid file root name"));
        }
        let root = std::fs::canonicalize(path)?;
        if !root.is_dir() {
            return Err(Error::message(
                "Each file root must be an existing directory",
            ));
        }
        roots.insert(name.to_owned(), canonical_root_text(&root)?);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_config() -> Value {
        json!({ "gateway_url": "https://EXAMPLE.com:443", "device_id": "device_1",
                "token": "a".repeat(32), "roots": {"work": std::env::temp_dir()} })
    }

    #[cfg(windows)]
    #[test]
    fn local_canonical_roots_use_the_accepted_device_config_spelling() {
        let roots = local_roots(&[format!("work={}", std::env::temp_dir().display())]).unwrap();
        assert!(valid_windows_root(&roots["work"]));
        assert!(!roots["work"].starts_with(r"\\?\"));
        let mut config = base_config();
        config["roots"] = serde_json::to_value(roots).unwrap();
        assert!(DeviceConfig::parse(config).is_ok());
    }

    #[test]
    fn validates_urls_and_accepts_only_loopback_http() {
        assert_eq!(
            validate_gateway("https://EXAMPLE.com:443").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            validate_gateway("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            validate_gateway("http://[::1]:8080").unwrap(),
            "http://[::1]:8080"
        );
        for bad in [
            "http://example.com",
            "http://127.0.0.2",
            "https://user:pass@example.com",
            "https://example.com/a",
            "https://example.com/?x=1",
            " https://example.com",
        ] {
            assert!(validate_gateway(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn config_defaults_reject_unknown_fields_and_accept_integer_float() {
        let config = DeviceConfig::parse(base_config()).unwrap();
        assert_eq!(config.gateway_url, "https://example.com");
        assert!(!config.allow_shell && !config.allow_write);
        assert_eq!(config.max_processes, 4);
        let mut value = base_config();
        value["max_processes"] = json!(4.0);
        assert_eq!(DeviceConfig::parse(value.clone()).unwrap().max_processes, 4);
        value["max_processes"] = json!(4.5);
        assert!(DeviceConfig::parse(value).is_err());
        let mut value = base_config();
        value["unexpected"] = json!(true);
        assert!(DeviceConfig::parse(value).is_err());
    }

    #[test]
    fn windows_root_syntax_matches_legacy_drive_and_unc_contract() {
        for root in [
            "C:/work",
            r"C:\work",
            "//host/share/work",
            r"\\host\share",
            "//host/中文",
        ] {
            assert!(valid_windows_root(root), "{root:?}");
        }
        for root in [
            "C:work",
            "/work",
            r"\work",
            "//host",
            "//host/",
            "//host//share",
            "//?/C:/work",
            r"\\.\pipe\agent",
            r"\??\C:\work",
            "//../share",
            "//host/share?",
            "//ho\nst/share",
            "C:/work\0file",
        ] {
            assert!(!valid_windows_root(root), "{root:?}");
        }
        assert!(!valid_windows_root(&format!("C:/{}", "a".repeat(4094))));
    }

    #[test]
    fn windows_system_root_syntax_matches_legacy_default_shell_contract() {
        for (root, shell) in [
            (r"C:\Windows", r"C:\Windows\System32\cmd.exe"),
            ("C:\\", r"C:\System32\cmd.exe"),
            (
                "c:\\\\Windows//Folder\\",
                r"c:\Windows\Folder\System32\cmd.exe",
            ),
            (r"C:\中文", r"C:\中文\System32\cmd.exe"),
        ] {
            assert_eq!(windows_default_shell(root).as_deref(), Some(shell));
        }
        for root in [
            "C:/Windows",
            "C:Windows",
            r"\\server\share",
            r"C:\Windows\..\else",
            r"C:\Windows\.\else",
            r"C:\Windows:stream",
            "C:\\Win\n",
            "C:\\Win\0",
        ] {
            assert!(windows_default_shell(root).is_none(), "{root:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_device_config_rejects_invalid_roots() {
        let mut value = base_config();
        value["shell"] = json!(r"C:\Windows\System32\cmd.exe");
        for root in [r"C:\work", r"\\host\share"] {
            value["roots"]["work"] = json!(root);
            assert!(DeviceConfig::parse(value.clone()).is_ok(), "{root}");
        }
        for root in ["//host", "//?/C:/work", r"\\.\pipe\agent"] {
            value["roots"]["work"] = json!(root);
            assert!(DeviceConfig::parse(value.clone()).is_err(), "{root}");
        }
    }

    #[test]
    fn config_filenames_match_sha256_contract() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let paths =
            config_paths(Path::new("/tmp/config"), "https://example.com", "default").unwrap();
        let expected = &sha256_hex(b"https://example.com")[..16];
        assert_eq!(
            paths[0],
            Path::new("/tmp/config/remote-device-hub").join(format!("{expected}-default.json"))
        );
        assert!(config_paths(Path::new("/tmp"), "https://example.com", "../bad").is_err());
    }
}
