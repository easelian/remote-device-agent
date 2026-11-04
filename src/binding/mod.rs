//! One selected local binding. Lifecycle intent survives a failed Hub request.
mod local;
mod unbind;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_SERVER, DeviceConfig, ProfilePaths, config_base, local_roots};
use crate::error::{Error, Result};
use crate::{profile_lock::ProfileLocks, storage};

pub(crate) use local::Running;
pub use unbind::unbind;

/// Runtime inputs are resolved locally, not accepted as CLI parameters.
pub struct Options {
    pub server: String,
    pub config: PathBuf,
    pub roots: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    roots: BTreeMap<String, String>,
}

pub(crate) fn sidecar(config: &Path, suffix: &str) -> PathBuf {
    let mut path = config.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match storage::read_private(path) {
        Ok(value) => Ok(Some(value)),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn remove_private(path: &Path) -> Result<()> {
    if read_optional(path)?.is_some() {
        std::fs::remove_file(path)?;
        #[cfg(unix)]
        storage::sync_directory(&std::fs::File::open(
            path.parent()
                .ok_or_else(|| Error::message("Invalid state path"))?,
        )?)?;
    }
    Ok(())
}

pub(crate) fn pending_unbind(config: &Path) -> Result<bool> {
    let Some(raw) = read_optional(&sidecar(config, ".unbind.json"))? else {
        return Ok(false);
    };
    let value: serde_json::Value = serde_json::from_slice(&raw)?;
    if value != serde_json::json!({"version":1,"action":"unbind"}) {
        return Err(Error::message(
            "Invalid pending unbind record; inspect local state",
        ));
    }
    Ok(true)
}

pub(crate) fn request_unbind(config: &Path) -> Result<()> {
    storage::write_private_replace(
        &sidecar(config, ".unbind.json"),
        br#"{"version":1,"action":"unbind"}"#,
    )
}

/// Resolve the saved Hub before selecting the default configuration. Existing
/// default configurations keep their path; callers never have to choose it.
pub fn prepare_options(choose_directory: bool) -> Result<Options> {
    let base = config_base()?;
    let settings_path = base.join("remote-device-hub/settings.json");
    let path = ProfilePaths::resolve(&base, DEFAULT_SERVER, "default", None)?.destination;
    let mut options = Options {
        server: DEFAULT_SERVER.into(),
        config: path.clone(),
        roots: BTreeMap::new(),
    };
    if !choose_directory {
        return Ok(options);
    }
    let saved = read_optional(&settings_path)?
        .map(|raw| serde_json::from_slice::<Settings>(&raw))
        .transpose()?;
    let roots = if let Some(raw) = read_optional(&path)? {
        let config = DeviceConfig::parse(serde_json::from_slice(&raw)?)?;
        options.server = config.gateway_url.clone();
        config.roots
    } else if let Some(saved) = saved.as_ref().filter(|s| !s.roots.is_empty()) {
        local_roots(
            &saved
                .roots
                .iter()
                .map(|(name, path)| format!("{name}={path}"))
                .collect::<Vec<_>>(),
        )?
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(Error::message(
                "Run rdh once in an interactive terminal to choose the directory to share",
            ));
        }
        let cwd = std::env::current_dir()?;
        println!(
            "Choose the directory this device may expose. File tools stay within this directory; shell access, if approved, uses your OS account permissions."
        );
        print!("Directory [{}]: ", cwd.display());
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Err(Error::message("Directory selection cancelled"));
        }
        let directory = if line.trim().is_empty() {
            cwd.to_string_lossy().into_owned()
        } else {
            line.trim().to_owned()
        };
        local_roots(&[format!("work={directory}")])?
    };
    options.roots = roots;
    if read_optional(&path)?.is_none()
        && let Some(raw) = read_optional(&sidecar(&path, ".pairing.json"))?
    {
        let pending: serde_json::Value = serde_json::from_slice(&raw)?;
        options.server = crate::config::validate_gateway(
            pending["server"]
                .as_str()
                .ok_or_else(|| Error::message("Invalid pending Hub"))?,
        )?;
    }
    Ok(options)
}

pub(crate) fn save_roots(roots: &BTreeMap<String, String>) -> Result<()> {
    let path = config_base()?.join("remote-device-hub/settings.json");
    storage::ensure_parent(&path)?;
    storage::write_private_replace(
        &path,
        &serde_json::to_vec_pretty(&Settings {
            roots: roots.clone(),
        })?,
    )
}

pub(crate) fn acquire(paths: &[PathBuf]) -> Result<ProfileLocks> {
    match ProfileLocks::acquire(paths) {
        Ok(locks) => Ok(locks),
        Err(_) => {
            // Recovery proves the original Agent process has exited. It does
            // not replay its operations or claim that external effects stopped.
            ProfileLocks::recover(paths)?;
            ProfileLocks::acquire(paths)
        }
    }
}

pub(crate) fn report_existing(config: &Path) -> Result<bool> {
    local::report_existing(config)
}

pub(crate) async fn finish_pending(config: &Path, paths: &[PathBuf]) -> Result<()> {
    unbind::complete(config, paths).await
}

pub async fn update() -> Result<()> {
    let options = prepare_options(false)?;
    if report_existing(&options.config)? {
        return Err(Error::message(
            "Stop the running Agent before updating; its binding and automatic startup settings will be preserved",
        ));
    }
    if pending_unbind(&options.config)? {
        return Err(Error::message(
            "Finish the pending unbind with rdh unbind before updating",
        ));
    }
    let _locks = acquire(std::slice::from_ref(&options.config))?;
    crate::self_update::run().await
}
