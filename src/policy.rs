//! Validated private hardening policy and pre-operation denials.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::DeviceConfig;
use crate::error::{Error, Result};
use crate::storage;

pub const KNOWN_OPERATIONS: &[&str] = &[
    "device_info",
    "get_operation_status",
    "get_startup",
    "set_startup",
    "list_directory",
    "read_file",
    "write_file",
    "edit_file",
    "make_directory",
    "search_files",
    "file_info",
    "read_file_chunk",
    "read_multiple_files",
    "move_file",
    "start_process",
    "read_process",
    "list_processes",
    "write_process_stdin",
    "terminate_process",
    "start_search",
    "get_search_results",
    "stop_search",
    "list_searches",
];
pub const WRITE_OPERATIONS: &[&str] = &["write_file", "edit_file", "make_directory", "move_file"];

#[derive(Clone)]
pub struct AuditSettings {
    pub enabled: bool,
    pub required: bool,
    pub max_bytes: u64,
    pub max_files: usize,
}

#[derive(Clone)]
pub struct Sandbox {
    pub mode: String,
    pub network: Option<bool>,
    pub hide_home: Option<bool>,
    pub expose: Vec<String>,
    pub limits: BTreeMap<String, u64>,
}

#[derive(Clone)]
pub struct Policy {
    pub allowed: Option<BTreeSet<String>>,
    pub readonly_roots: BTreeSet<String>,
    protected: Vec<Vec<String>>,
    pub sandbox: Option<Sandbox>,
    pub audit: AuditSettings,
    pub inherit_env: Vec<String>,
}

pub struct Denial {
    pub code: &'static str,
    pub message: String,
}

fn reject_unknown(map: &Map<String, Value>, keys: &[&str], label: &str) -> Result<()> {
    if let Some(key) = map.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(Error::message(format!(
            "{label} contains an unknown field: {key}"
        )));
    }
    Ok(())
}

fn strings(value: Option<&Value>, label: &str, max: usize) -> Result<Vec<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .filter(|array| array.len() <= max)
        .ok_or_else(|| {
            Error::message(format!("{label} must be an array of at most {max} strings"))
        })?;
    array
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                Error::message(format!("{label} must be an array of at most {max} strings"))
            })
        })
        .collect()
}

fn bool_value(map: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match map.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(Error::message("Invalid audit configuration parameters")),
    }
}

fn integer(value: &Value, min: u64, max: u64) -> Option<u64> {
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0 && number >= min as f64 && number <= max as f64)
        .then_some(number as u64)
}

fn name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn parts(path: &str) -> std::result::Result<Vec<String>, String> {
    if path.encode_utf16().count() > 4096 || path.starts_with('/') || path.contains('\0') {
        return Err("Path must be relative and contain no NUL bytes".into());
    }
    let mut components = Vec::new();
    for part in path.split('/') {
        if part == ".." {
            return Err("Path must be relative to a root, with no '..' or NUL".into());
        }
        if !part.is_empty() && part != "." {
            #[cfg(any(windows, target_os = "macos"))]
            components.push(part.to_lowercase());
            #[cfg(not(any(windows, target_os = "macos")))]
            components.push(part.to_owned());
        }
    }
    Ok(components)
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allowed: None,
            readonly_roots: BTreeSet::new(),
            protected: Vec::new(),
            sandbox: None,
            audit: AuditSettings {
                enabled: true,
                required: false,
                max_bytes: 5 * 1024 * 1024,
                max_files: 3,
            },
            inherit_env: Vec::new(),
        }
    }
}

pub fn sidecar_path(config: &Path) -> PathBuf {
    if config.extension().is_some_and(|value| value == "json") {
        let mut path = config.to_path_buf();
        path.set_extension("policy.json");
        path
    } else {
        let mut path = config.as_os_str().to_os_string();
        path.push(".policy.json");
        PathBuf::from(path)
    }
}

impl Policy {
    pub fn load(config_path: &Path, config: &DeviceConfig) -> Result<Self> {
        let bytes = match storage::read_private(&sidecar_path(config_path)) {
            Ok(bytes) => bytes,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => {
                return Err(Error::message(
                    "Cannot safely read the hardening configuration (mode 0600 required; symbolic links are not allowed)",
                ));
            }
        };
        if bytes.len() > 64 * 1024 {
            return Err(Error::message("The hardening configuration is too large"));
        }
        Self::parse(&serde_json::from_slice(&bytes)?, config)
    }

    pub fn parse(value: &Value, config: &DeviceConfig) -> Result<Self> {
        let map = value
            .as_object()
            .ok_or_else(|| Error::message("Invalid hardening configuration"))?;
        reject_unknown(
            map,
            &[
                "version",
                "allowed_operations",
                "readonly_roots",
                "protected_paths",
                "sandbox",
                "audit",
                "inherit_env",
            ],
            "Hardening configuration",
        )?;
        if map.get("version").and_then(Value::as_f64) != Some(1.0) {
            return Err(Error::message(
                "The hardening configuration version must be 1",
            ));
        }
        let readonly = strings(map.get("readonly_roots"), "readonly_roots", 64)?;
        for root in &readonly {
            if !name(root) || !config.roots.contains_key(root) {
                return Err(Error::message(format!(
                    "readonly_roots references an unconfigured root: {root}"
                )));
            }
        }
        let allowed = if map.get("allowed_operations").is_none_or(Value::is_null) {
            None
        } else {
            let mut allowed: BTreeSet<_> = strings(
                map.get("allowed_operations"),
                "allowed_operations",
                KNOWN_OPERATIONS.len(),
            )?
            .into_iter()
            .collect();
            if allowed
                .iter()
                .any(|op| !KNOWN_OPERATIONS.contains(&op.as_str()))
            {
                return Err(Error::message("Unknown operation in allowed_operations"));
            }
            allowed.insert("device_info".into());
            allowed.insert("get_operation_status".into());
            Some(allowed)
        };
        let protected = strings(map.get("protected_paths"), "protected_paths", 64)?
            .into_iter()
            .map(|path| {
                if path.is_empty()
                    || path.encode_utf16().count() > 256
                    || path.starts_with('/')
                    || path.contains('\0')
                {
                    return Err(Error::message("Invalid protected_paths entry"));
                }
                let components =
                    parts(&path).map_err(|_| Error::message("Invalid protected_paths entry"))?;
                if components.is_empty() {
                    return Err(Error::message("Invalid protected_paths entry"));
                }
                Ok(components)
            })
            .collect::<Result<Vec<_>>>()?;

        let sandbox = if let Some(value) = map.get("sandbox") {
            let sandbox = value
                .as_object()
                .ok_or_else(|| Error::message("Invalid sandbox configuration"))?;
            reject_unknown(
                sandbox,
                &["mode", "network", "hide_home", "expose", "limits"],
                "sandbox",
            )?;
            let mode = sandbox
                .get("mode")
                .and_then(Value::as_str)
                .filter(|value| ["none", "bwrap"].contains(value))
                .ok_or_else(|| Error::message("sandbox.mode must be none or bwrap"))?;
            let limits = match sandbox.get("limits") {
                None | Some(Value::Null) => BTreeMap::new(),
                Some(Value::Object(values)) => {
                    let mut limits = BTreeMap::new();
                    for (key, value) in values {
                        let range = match key.as_str() {
                            "cpu_seconds" => (1, 86400),
                            "memory_kb" => (65536, 1 << 30),
                            "open_files" => (16, 1 << 20),
                            "file_size_blocks" => (1, 1 << 30),
                            _ => {
                                return Err(Error::message(format!(
                                    "Invalid sandbox.limits.{key}"
                                )));
                            }
                        };
                        limits.insert(
                            key.clone(),
                            integer(value, range.0, range.1).ok_or_else(|| {
                                Error::message(format!("Invalid sandbox.limits.{key}"))
                            })?,
                        );
                    }
                    limits
                }
                _ => return Err(Error::message("Invalid sandbox.limits")),
            };
            if mode == "none" {
                if ["network", "hide_home", "expose"]
                    .iter()
                    .any(|key| sandbox.contains_key(*key))
                {
                    return Err(Error::message(
                        "sandbox.network, hide_home, and expose are only valid when mode is bwrap",
                    ));
                }
                Some(Sandbox {
                    mode: mode.into(),
                    network: None,
                    hide_home: None,
                    expose: Vec::new(),
                    limits,
                })
            } else {
                let network = sandbox
                    .get("network")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        Error::message("sandbox.network must be explicitly set to true or false")
                    })?;
                let hide_home = match sandbox.get("hide_home") {
                    None => true,
                    Some(Value::Bool(value)) => *value,
                    _ => return Err(Error::message("sandbox.hide_home must be a boolean")),
                };
                let expose = strings(sandbox.get("expose"), "sandbox.expose", 32)?;
                if expose
                    .iter()
                    .any(|path| !Path::new(path).is_absolute() || path.contains('\0'))
                {
                    return Err(Error::message(
                        "sandbox.expose must contain at most 32 absolute paths",
                    ));
                }
                Some(Sandbox {
                    mode: mode.into(),
                    network: Some(network),
                    hide_home: Some(hide_home),
                    expose,
                    limits,
                })
            }
        } else {
            None
        };

        let mut audit = Self::default().audit;
        if let Some(value) = map.get("audit") {
            let settings = value
                .as_object()
                .ok_or_else(|| Error::message("Invalid audit configuration"))?;
            reject_unknown(
                settings,
                &["enabled", "required", "max_bytes", "max_files"],
                "audit",
            )?;
            audit.enabled = bool_value(settings, "enabled", true)?;
            audit.required = bool_value(settings, "required", false)?;
            if let Some(value) = settings.get("max_bytes") {
                audit.max_bytes = integer(value, 4096, 9_007_199_254_740_991)
                    .ok_or_else(|| Error::message("Invalid audit configuration parameters"))?;
            }
            if let Some(value) = settings.get("max_files") {
                audit.max_files = integer(value, 1, 20)
                    .ok_or_else(|| Error::message("Invalid audit configuration parameters"))?
                    as usize;
            }
            if audit.required && !audit.enabled {
                return Err(Error::message(
                    "audit.required requires audit.enabled to be true",
                ));
            }
        }
        let mut inherit_env = Vec::new();
        for value in strings(map.get("inherit_env"), "inherit_env", 64)? {
            if value.is_empty()
                || value.len() > 128
                || !value.as_bytes()[0].is_ascii_alphabetic() && value.as_bytes()[0] != b'_'
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(Error::message(
                    "inherit_env contains an invalid variable name",
                ));
            }
            if !inherit_env.contains(&value) {
                inherit_env.push(value);
            }
        }
        Ok(Self {
            allowed,
            readonly_roots: readonly.into_iter().collect(),
            protected,
            sandbox,
            audit,
            inherit_env,
        })
    }

    pub fn ensure_sandbox_supported(&self) -> Result<()> {
        if self.sandbox.as_ref().is_some_and(|sandbox| {
            (sandbox.mode == "bwrap" && !cfg!(target_os = "linux"))
                || (!sandbox.limits.is_empty() && !cfg!(unix))
        }) {
            return Err(Error::message(
                "The configured sandbox or process limits are unsupported on this platform",
            ));
        }
        Ok(())
    }

    pub fn protected_paths(&self) -> Vec<String> {
        self.protected.iter().map(|parts| parts.join("/")).collect()
    }

    pub fn denial(&self, operation: &str, params: &Value) -> Option<Denial> {
        if self
            .allowed
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(operation))
        {
            return Some(Denial {
                code: "policy_operation_denied",
                message: format!(
                    "Policy denied: operation {operation} is not allowed on this device"
                ),
            });
        }
        if !WRITE_OPERATIONS.contains(&operation) {
            return None;
        }
        let map = params.as_object()?;
        let targets = if operation == "move_file" {
            let modern = map.contains_key("source") || map.contains_key("destination");
            if modern
                && ["path", "destination_root", "destination_path"]
                    .iter()
                    .any(|key| map.contains_key(*key))
            {
                return Some(Denial {
                    code: "policy_parameters_invalid",
                    message: "Policy denied: Mixed move parameter formats are not allowed".into(),
                });
            }
            let root = map.get("root").and_then(Value::as_str);
            let source = map
                .get(if modern { "source" } else { "path" })
                .and_then(Value::as_str);
            let destination_root = if modern {
                root
            } else {
                map.get("destination_root").and_then(Value::as_str)
            };
            let destination = map
                .get(if modern {
                    "destination"
                } else {
                    "destination_path"
                })
                .and_then(Value::as_str);
            if root.is_none()
                || source.is_none()
                || destination_root.is_none()
                || destination.is_none()
            {
                return Some(Denial {
                    code: "policy_parameters_invalid",
                    message: "Policy denied: Invalid move parameters".into(),
                });
            }
            vec![(root, source), (destination_root, destination)]
        } else {
            vec![(
                map.get("root").and_then(Value::as_str),
                map.get("path").and_then(Value::as_str),
            )]
        };
        // Node validates every target path before checking any root or segment.
        let mut checked = Vec::new();
        for (root, path) in targets {
            let path = match path.map(parts).transpose() {
                Ok(value) => value,
                Err(error) => {
                    return Some(Denial {
                        code: "policy_parameters_invalid",
                        message: format!("Policy denied: {error}"),
                    });
                }
            };
            checked.push((root, path));
        }
        for (root, path) in checked {
            if let Some(root) = root.filter(|root| self.readonly_roots.contains(*root)) {
                return Some(Denial {
                    code: "policy_root_readonly",
                    message: format!("Policy denied: root {root} is read-only"),
                });
            }
            if let Some(path) = path {
                for protected in &self.protected {
                    if path
                        .windows(protected.len())
                        .any(|window| window == protected)
                    {
                        return Some(Denial {
                            code: "policy_path_protected",
                            message: format!(
                                "Policy denied: {} is a protected path",
                                protected.join("/")
                            ),
                        });
                    }
                }
            }
        }
        None
    }

    pub fn effective_operations(&self, implemented: &[&str], config: &DeviceConfig) -> Vec<String> {
        KNOWN_OPERATIONS
            .iter()
            .filter(|operation| implemented.contains(operation))
            .filter(|operation| {
                self.allowed
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(**operation))
            })
            .filter(|operation| config.allow_write || !WRITE_OPERATIONS.contains(operation))
            .filter(|operation| config.allow_shell || **operation != "start_process")
            .map(|operation| (*operation).to_owned())
            .collect()
    }

    pub fn describe(&self) -> Value {
        json!({"allowed_operations": self.allowed, "readonly_roots":self.readonly_roots, "protected_paths":self.protected_paths()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DeviceConfig {
        let temp = std::env::temp_dir();
        DeviceConfig::parse(
            json!({"gateway_url":"http://localhost:80","device_id":"d","token":"x".repeat(32),
            "roots":{"work":temp.join("work"), "docs":temp.join("docs")}, "allow_write":true}),
        )
        .unwrap()
    }

    #[test]
    fn sidecar_requires_strict_schema_and_existing_roots() {
        assert!(Policy::parse(&json!({"version":1,"unexpected":true}), &config()).is_err());
        assert!(
            Policy::parse(
                &json!({"version":1,"readonly_roots":["missing"]}),
                &config()
            )
            .is_err()
        );
        assert!(
            Policy::parse(
                &json!({"version":1,"audit":{"required":true,"enabled":false}}),
                &config()
            )
            .is_err()
        );
        assert_eq!(
            Policy::parse(
                &json!({"version":1,"sandbox":{"mode":"bwrap","network":false}}),
                &config()
            )
            .unwrap()
            .ensure_sandbox_supported()
            .is_ok(),
            cfg!(target_os = "linux")
        );
        assert_eq!(
            sidecar_path(Path::new("/tmp/device.json")),
            Path::new("/tmp/device.policy.json")
        );
    }

    #[test]
    fn policy_checks_allowlist_both_move_paths_and_protected_segments() {
        let policy = Policy::parse(
            &json!({"version":1,"allowed_operations":["read_file","write_file","move_file"],
            "readonly_roots":["docs"], "protected_paths":[".git/hooks", ".env"]}),
            &config(),
        )
        .unwrap();
        assert!(policy.denial("device_info", &json!({})).is_none());
        assert_eq!(
            policy.denial("start_process", &json!({})).unwrap().code,
            "policy_operation_denied"
        );
        assert_eq!(
            policy
                .denial("write_file", &json!({"root":"docs","path":"ok"}))
                .unwrap()
                .code,
            "policy_root_readonly"
        );
        assert_eq!(
            policy
                .denial(
                    "write_file",
                    &json!({"root":"work","path":"project/.git/hooks/pre"})
                )
                .unwrap()
                .code,
            "policy_path_protected"
        );
        assert_eq!(
            policy
                .denial(
                    "move_file",
                    &json!({"root":"work","source":"a","destination":".env"})
                )
                .unwrap()
                .code,
            "policy_path_protected"
        );
        assert_eq!(
            policy
                .denial(
                    "move_file",
                    &json!({"root":"work","source":"a","destination":"b","path":"a"})
                )
                .unwrap()
                .code,
            "policy_parameters_invalid"
        );
        assert!(
            policy
                .denial("write_file", &json!({"root":"work","path":".env.example"}))
                .is_none()
        );
    }
}
