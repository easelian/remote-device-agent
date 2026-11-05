use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

use crate::audit::AuditLog;
use crate::binding::{self, Options};
use crate::config::{DeviceConfig, home_directory, same_hub};
use crate::error::{Error, Result};
use crate::network::Network;
use crate::policy::Policy;
use crate::rpc::Runtime;
use crate::startup::{STARTUP_OPERATIONS, Startup};
use crate::{pairing, session};

fn audit_directory() -> Result<PathBuf> {
    let base = if let Some(base) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(base)
    } else {
        #[cfg(windows)]
        {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or(home_directory()?.join("AppData/Local"))
        }
        #[cfg(not(windows))]
        {
            home_directory()?.join(".local/state")
        }
    };
    Ok(base.join("remote-device-hub/audit"))
}

pub async fn launch(
    options: Options,
    shutdown: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
) -> Result<()> {
    let path = &options.config;
    let targets = std::slice::from_ref(path);
    if binding::pending_unbind(path)? {
        binding::finish_pending(path, targets).await?;
        println!("Pending unbind completed. Run rdh again to bind this device.");
        return Ok(());
    }
    crate::startup::confirm_pending(path)?;
    if binding::report_existing(path)? {
        return Ok(());
    }
    let mut locks = binding::acquire(targets)?;
    // Recheck under the instance lock: unbind can race with startup.
    if binding::pending_unbind(path)? {
        return Ok(());
    }
    let running = binding::Running::start(path, stop)?;
    let network = Network::from_env()?;
    let config = match DeviceConfig::read(path) {
        Ok(config) => {
            if !same_hub(&config.gateway_url, &options.server) {
                return Err(Error::message(
                    "The saved binding belongs to a different Hub",
                ));
            }
            binding::save_roots(&config.roots)?;
            println!("Using saved binding: {}", config.device_id);
            config
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            if options.roots.is_empty() {
                return Err(Error::message("Choose a local directory before binding"));
            }
            binding::save_roots(&options.roots)?;
            println!("Target device: {}", crate::system::info().hostname);
            println!(
                "File roots: {}",
                options
                    .roots
                    .values()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let config = match pairing::pair(
                &network,
                &options.server,
                &options.roots,
                path,
                &running,
                shutdown.clone(),
            )
            .await
            {
                Ok(config) => config,
                Err(_) if *shutdown.borrow() => return Ok(()),
                Err(error) => return Err(error),
            };
            config.write_new(path)?;
            binding::remove_private(&binding::sidecar(path, ".pairing.json"))?;
            println!("Device bound: {}", config.device_id);
            config
        }
        Err(error) => return Err(error),
    };
    if *shutdown.borrow() || binding::pending_unbind(path)? {
        return Ok(());
    }
    let policy = Policy::load(path, &config)?;
    policy.ensure_sandbox_supported()?;
    let audit = if policy.audit.enabled {
        match audit_directory().and_then(|directory| {
            AuditLog::open(
                &directory,
                &config.device_id,
                policy.audit.max_bytes,
                policy.audit.max_files,
                policy.audit.required,
            )
            .map_err(Error::message)
        }) {
            Ok(audit) => Some(Arc::new(audit)),
            Err(error) if policy.audit.required => {
                return Err(Error::message(format!(
                    "The required audit log is unavailable: {error}"
                )));
            }
            Err(_) => {
                eprintln!("Local audit is unavailable; audit health will report it as disabled.");
                None
            }
        }
    } else {
        None
    };
    let startup = if STARTUP_OPERATIONS.is_empty() {
        None
    } else {
        Some(Arc::new(Startup::new(path)?))
    };
    let runtime = Runtime::with_config_path(config.clone(), policy, audit, startup, Some(path))?;
    let browser = runtime.info()["capabilities"]["browser"].clone();
    let browser_status = match browser["reason"].as_str() {
        Some("disabled_by_local_policy") => "disabled in local policy",
        Some("browser_not_installed") => "no configured Chromium executable was found",
        Some("platform_not_supported") => "not supported on this platform",
        Some(_) => "private browser storage is unavailable",
        None => "enabled; launch is checked when a session is opened",
    };
    println!("AI browser sessions: {browser_status}.");
    if cfg!(any(
        target_os = "linux",
        target_os = "macos",
        all(
            windows,
            feature = "experimental-windows-private-storage",
            feature = "experimental-windows-processes"
        )
    )) {
        println!(
            "Local browser policy: {} (restart after changes).",
            crate::policy::sidecar_path(path).display()
        );
    }
    running.status(json!({"state":"bound","device_id":config.device_id}))?;
    println!("Connecting to the Hub. Keep this terminal open; press Ctrl+C to stop.");
    let outcome = session::run(&network, &config, &runtime, shutdown).await;
    if !runtime.shutdown().await {
        locks.retain_on_drop();
        return Err(Error::message(
            "Agent shutdown did not finish; operation outcomes may be unknown. Inspect the previous Agent and its tasks before restarting.",
        ));
    }
    let revoked = matches!(outcome, Err(Error::AuthorizationRejected(_)))
        && network.binding_missing(&config).await.unwrap_or(false);
    if revoked && !binding::pending_unbind(path)? {
        let command = binding::sidecar(path, ".unbind-command");
        let _command = binding::acquire(std::slice::from_ref(&command))?;
        binding::request_unbind(path)?;
        crate::startup::disable_for_unbind(path)?;
        crate::browser::remove_binding(path, &config.token)?;
        binding::remove_private(path)?;
        binding::remove_private(&binding::sidecar(path, ".pairing.json"))?;
        binding::remove_private(&binding::sidecar(path, ".unbind.json"))?;
        println!(
            "The Hub revoked this binding. Automatic startup is disabled. Run rdh to bind again."
        );
    } else if !binding::pending_unbind(path)? {
        outcome?;
    }
    drop(running);
    locks.release()?;
    Ok(())
}
