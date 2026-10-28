use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::watch;

use crate::audit::AuditLog;
use crate::cli::LaunchArgs;
use crate::config::{
    DeviceConfig, ProfilePaths, config_base, home_directory, local_roots, same_hub,
};
use crate::error::{Error, Result};
use crate::install_lock::RunningInstall;
use crate::network::Network;
use crate::policy::Policy;
use crate::profile_lock::ProfileLocks;
use crate::rpc::Runtime;
use crate::startup::{STARTUP_OPERATIONS, Startup};
use crate::{pairing, session, system};

fn audit_directory() -> Result<PathBuf> {
    let base =
        if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
            PathBuf::from(value)
        } else {
            #[cfg(windows)]
            {
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or(home_directory()?.join("AppData").join("Local"))
            }
            #[cfg(not(windows))]
            {
                home_directory()?.join(".local").join("state")
            }
        };
    Ok(base.join("remote-device-hub").join("audit"))
}

/// Check the configuration in the candidate binary without contacting the Hub
/// or modifying persistent state. The updater separately holds profile locks.
pub fn validate_config(args: &[String]) -> Result<()> {
    let [flag, config_path] = args else {
        return Err(Error::message("Use validate-config --config ABSOLUTE_PATH"));
    };
    let path = Path::new(config_path);
    if flag != "--config" || !path.is_absolute() {
        return Err(Error::message("Use validate-config --config ABSOLUTE_PATH"));
    }
    let config = DeviceConfig::read(path)?;
    let policy = Policy::load(path, &config)?;
    policy.ensure_sandbox_supported()?;
    let _files =
        crate::files::Files::new(&config.roots, config.allow_write).map_err(Error::message)?;
    let _sandbox = crate::sandbox::Sandbox::prepare(&config, &policy, Some(path))?;
    if !STARTUP_OPERATIONS.is_empty() {
        let _startup = Startup::new(path)?;
    }
    Ok(())
}

pub async fn launch(
    args: LaunchArgs,
    shutdown: watch::Receiver<bool>,
    stop: watch::Sender<bool>,
) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    let _ = stop;
    let mut managed = RunningInstall::acquire_launch_current()?;
    let network = Network::from_env()?;
    if std::env::var_os("NODE_USE_ENV_PROXY").is_some()
        || std::env::var_os("NODE_OPTIONS").is_some()
    {
        eprintln!(
            "Node network startup settings are not used by the Rust Agent; configure this device's Rust network settings if needed."
        );
    }
    let base = if args.config.is_some() {
        None
    } else {
        Some(config_base()?)
    };
    let paths = ProfilePaths::resolve(
        base.as_deref().unwrap_or_else(|| Path::new("")),
        &args.server,
        &args.profile,
        args.config.as_deref(),
    )?;
    if let Some(installation) = managed.as_ref() {
        installation.check_launch_profile(&paths.destination, args.pair_only)?;
    }
    if args.recover_lock {
        ProfileLocks::recover(&paths.lock_targets)?;
    }
    let mut locks = ProfileLocks::acquire(&paths.lock_targets)?;
    let loaded = DeviceConfig::read(&paths.destination);
    let mut config = match loaded {
        Ok(config) => {
            if !same_hub(&config.gateway_url, &args.server) {
                return Err(Error::message(
                    "The existing configuration belongs to another Hub. Choose a different --profile.",
                ));
            }
            if !args.roots.is_empty() && config.roots != local_roots(&args.roots)? {
                return Err(Error::message(
                    "This profile is paired with different file roots. Use --profile to create another profile.",
                ));
            }
            println!("Using saved pairing: {}", config.device_id);
            config
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let roots = local_roots(&args.roots)?;
            println!("Target device: {}", system::info().hostname);
            println!(
                "File roots: {}",
                roots.values().cloned().collect::<Vec<_>>().join(", ")
            );
            let config = match pairing::pair(
                &network,
                &args.server,
                &roots,
                args.name.as_deref(),
                args.no_browser,
                shutdown.clone(),
            )
            .await
            {
                Ok(config) => config,
                Err(_) if *shutdown.borrow() => return Ok(()),
                Err(error) => return Err(error),
            };
            config.write_new(&paths.destination)?;
            println!(
                "Device paired: {}\nConfiguration saved to: {}",
                config.device_id,
                paths.destination.display()
            );
            config
        }
        Err(error) => return Err(error),
    };
    if let Some(installation) = managed.as_ref() {
        installation.register_profile(&paths.destination)?;
    }
    if args.pair_only {
        if let Some(installation) = managed.as_mut() {
            installation.finish_launch();
        }
        println!(
            "Pairing complete. Configuration saved to: {}",
            paths.destination.display()
        );
    } else {
        let policy = Policy::load(&paths.destination, &config)?;
        policy.ensure_sandbox_supported()?;
        let audit = if policy.audit.enabled {
            let opened = audit_directory().and_then(|directory| {
                AuditLog::open(
                    &directory,
                    &config.device_id,
                    policy.audit.max_bytes,
                    policy.audit.max_files,
                    policy.audit.required,
                )
                .map_err(Error::message)
            });
            match opened {
                Ok(audit) => Some(Arc::new(audit)),
                Err(error) if policy.audit.required => {
                    return Err(Error::message(format!(
                        "The required audit log is unavailable: {error}"
                    )));
                }
                Err(_) => {
                    eprintln!(
                        "Local audit is unavailable; audit health will report it as disabled."
                    );
                    None
                }
            }
        } else {
            None
        };
        config.gateway_url = args.server;
        let startup = if STARTUP_OPERATIONS.is_empty() {
            None
        } else {
            Some(Arc::new(Startup::new(&paths.destination)?))
        };
        let runtime = Runtime::with_config_path(
            config.clone(),
            policy,
            audit,
            startup,
            Some(&paths.destination),
        )?;
        #[cfg(target_os = "linux")]
        let trial = managed
            .as_ref()
            .and_then(RunningInstall::trial_launch)
            .cloned();
        #[cfg(target_os = "linux")]
        if trial.is_some() {
            runtime.quarantine_before_connect();
        }
        if let Some(installation) = managed.as_mut() {
            installation.mark_running(&paths.destination, args.recover_lock)?;
        }
        #[cfg(target_os = "linux")]
        let mut readiness = if let Some(installation) = managed.as_ref() {
            match crate::readiness::Reporter::start(
                installation.running_record()?,
                &config.device_id,
                &runtime.receipts.instance_id,
                Arc::clone(&runtime),
                stop,
                trial.clone(),
            ) {
                Ok(reporter) => Some(reporter),
                Err(error) if trial.is_some() => {
                    if let Some(installation) = managed.as_mut() {
                        installation.mark_clean()?;
                    }
                    return Err(Error::message(format!(
                        "A service update trial requires its local control socket: {error}"
                    )));
                }
                Err(_) => {
                    eprintln!(
                        "Local readiness probe is unavailable; online upgrade cannot be verified."
                    );
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let mut readiness: Option<crate::readiness::Reporter> = None;
        if let Some(installation) = managed.as_mut() {
            installation.finish_launch();
        }
        eprintln!(
            "Rust development build: user-systemd service updates are experimental and require explicit local selection."
        );
        println!("Connecting to the Hub. Keep this terminal open; press Ctrl+C to stop.");
        let outcome = session::run(&network, &config, &runtime, shutdown, readiness.as_ref()).await;
        if let Some(reporter) = readiness.take() {
            reporter.close().await;
        }
        if !runtime.shutdown().await {
            locks.retain_on_drop();
            return Err(Error::message(
                "Agent shutdown did not finish; operation outcomes may be unknown. Inspect before recovering the profile lock.",
            ));
        }
        if let Some(installation) = managed.as_mut() {
            installation.mark_clean()?;
        }
        outcome?;
    }
    locks.release()?;
    Ok(())
}
