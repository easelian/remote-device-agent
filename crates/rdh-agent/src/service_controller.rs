//! Coordinated user-systemd update of a native managed installation.
//! The durable intent is authoritative across controller crashes. Before a
//! commit, recovery restores the old version; after a commit, it rolls forward.

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::process::Command;

    use crate::error::{Error, Result};
    use crate::install_lock::{
        ControllerLock, LaunchBarrier, ManagerLock, PENDING, RunningInstall, pointer,
    };
    use crate::native_install;
    use crate::readiness::{
        Snapshot, activate_profile, probe_profile, stop_exact_idle, stop_trial,
    };
    use crate::service_update::{self, Intent, Phase};
    use crate::systemd_identity::{self, UnitIdentity};

    const READY_TIMEOUT: Duration = Duration::from_secs(30);
    const INACTIVE_TIMEOUT: Duration = Duration::from_secs(15);
    const POLL: Duration = Duration::from_millis(100);

    fn pending_error(error: impl std::fmt::Display) -> Error {
        Error::message(format!(
            "{error}; the service update state may need recovery. Inspect the service and run rdh upgrade --recover-service if a record remains"
        ))
    }

    async fn barrier(prefix: &Path) -> Result<LaunchBarrier> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match LaunchBarrier::acquire(prefix) {
                    Ok(lock) => return Ok(lock),
                    Err(_) => tokio::time::sleep(POLL).await,
                }
            }
        })
        .await
        .map_err(|_| Error::message("Another native Agent launch is in progress"))?
    }

    async fn wait_inactive(prefix: &Path, intent: &Intent) -> Result<()> {
        tokio::time::timeout(INACTIVE_TIMEOUT, async {
            loop {
                if systemd_identity::inspect_stopped(
                    prefix,
                    Path::new(&intent.config),
                    &intent.unit,
                    &intent.original_definition,
                )
                .await
                .is_ok()
                {
                    return;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .map_err(|_| Error::message("The stopped service or its definition could not be verified"))
    }

    async fn start(unit: &str) -> Result<()> {
        if !systemd_identity::valid_unit(unit) {
            return Err(Error::message("Invalid user systemd service name"));
        }
        let mut child = Command::new("/usr/bin/systemctl")
            .args(["--user", "--no-ask-password", "start", "--", unit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(_)) => Err(Error::message("User systemd did not confirm service start")),
            _ => Err(Error::message("Service start outcome is unknown")),
        }
    }

    fn expected_version(intent: &Intent) -> Result<&str> {
        match intent.phase {
            Phase::Trial | Phase::CommitTrial | Phase::AcceptedTrial => Ok(&intent.new_current),
            Phase::RollbackTrial | Phase::CommitRollback | Phase::AcceptedRollback => {
                Ok(&intent.old_current)
            }
            Phase::Preparing => Err(Error::message("No service trial was selected")),
        }
    }

    async fn ready_service(
        controller: &ControllerLock,
        intent: &Intent,
    ) -> Result<(RunningInstall, Snapshot, UnitIdentity)> {
        tokio::time::timeout(READY_TIMEOUT, async {
            loop {
                let executable = controller.prefix().join("current/rdh");
                if let Ok(guard) = RunningInstall::inspect_at(controller.prefix(), &executable) {
                    let config = Path::new(&intent.config);
                    if let Ok(identity) =
                        systemd_identity::inspect(&guard, controller.prefix(), config, &intent.unit)
                            .await
                    {
                        if let Ok(snapshot) =
                            probe_profile(&guard, controller.prefix(), config).await
                        {
                            if snapshot.ready
                                && snapshot.version == expected_version(intent)?
                                && identity.definition_hash == intent.original_definition
                                && !identity.transient
                                && identity.pid == snapshot.pid
                                && identity.instance_id == snapshot.instance_id
                                && identity.connection_generation == snapshot.connection_generation
                                && identity.quarantined == snapshot.quarantined
                            {
                                return Ok((guard, snapshot, identity));
                            }
                        }
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .map_err(|_| Error::message("The selected service did not become verifiably Hub-ready"))?
    }

    async fn finish_started(controller: &ControllerLock, intent: &Intent) -> Result<Intent> {
        let (guard, _, _) = ready_service(controller, intent).await?;
        let gate = barrier(controller.prefix()).await?;
        // Re-read both sources with the launch gate held so a new process
        // cannot inherit a stale decision while the journal is finalized.
        let config = Path::new(&intent.config);
        let identity =
            systemd_identity::inspect(&guard, controller.prefix(), config, &intent.unit).await?;
        let snapshot = probe_profile(&guard, controller.prefix(), config).await?;
        let committed = if snapshot.quarantined {
            let committed =
                service_update::commit(controller, &gate, &guard, intent, &snapshot, &identity)?;
            activate_profile(&guard, controller.prefix(), config, &snapshot).await?;
            committed
        } else {
            if !matches!(intent.phase, Phase::CommitTrial | Phase::CommitRollback) {
                return Err(Error::message("An uncommitted service trial accepted work"));
            }
            intent.clone()
        };
        let active_identity =
            systemd_identity::inspect(&guard, controller.prefix(), config, &intent.unit).await?;
        let active = probe_profile(&guard, controller.prefix(), config).await?;
        let accepted = service_update::accept(
            controller,
            &gate,
            &guard,
            &committed,
            &active,
            &active_identity,
        )?;
        service_update::finish(controller, &gate, &accepted)?;
        Ok(accepted)
    }

    fn original_matches(
        guard: &RunningInstall,
        intent: &Intent,
        snapshot: &Snapshot,
        identity: &UnitIdentity,
    ) -> Result<bool> {
        let records = guard.running_records()?;
        Ok(records.len() == 1
            && records[0].pid == intent.original.pid
            && records[0].nonce == intent.original.nonce
            && snapshot.pid == intent.original.pid
            && snapshot.instance_id == intent.original.instance_id
            && snapshot.version == intent.old_current
            && snapshot.ready
            && !snapshot.quarantined
            && identity.invocation_id == intent.original_invocation
            && identity.definition_hash == intent.original_definition
            && !identity.transient)
    }

    /// Stop an uncommitted instance only after verifying its exact local peer
    /// and systemd unit. The launch barrier remains held throughout.
    async fn quiescent_manager(
        controller: &ControllerLock,
        executable: &Path,
        intent: &Intent,
        recover_stale: bool,
    ) -> Result<(LaunchBarrier, ManagerLock)> {
        let gate = barrier(controller.prefix()).await?;
        if let Ok(manager) = ManagerLock::for_service(controller, executable, true, recover_stale) {
            return Ok((gate, manager));
        }
        let current_binary = controller.prefix().join("current/rdh");
        let guard = RunningInstall::inspect_at(controller.prefix(), &current_binary)?;
        let config = Path::new(&intent.config);
        match intent.phase {
            Phase::Preparing => {
                let identity =
                    systemd_identity::inspect(&guard, controller.prefix(), config, &intent.unit)
                        .await?;
                let snapshot = probe_profile(&guard, controller.prefix(), config).await?;
                if !original_matches(&guard, intent, &snapshot, &identity)? {
                    return Err(Error::message(
                        "The original service instance changed before recovery",
                    ));
                }
                stop_exact_idle(&guard, controller.prefix(), config, &snapshot).await?;
            }
            Phase::Trial => {
                let (snapshot, identity) = systemd_identity::inspect_trial(
                    &guard,
                    controller.prefix(),
                    config,
                    &intent.unit,
                )
                .await?;
                if snapshot.version != intent.new_current
                    || identity.definition_hash != intent.original_definition
                    || identity.transient
                {
                    return Err(Error::message(
                        "The service trial does not match the update record",
                    ));
                }
                stop_trial(&guard, controller.prefix(), config, &snapshot).await?;
            }
            _ => {
                return Err(Error::message(
                    "The service is not safe to stop during recovery",
                ));
            }
        }
        drop(guard);
        let manager = ManagerLock::for_service(controller, executable, true, recover_stale)?;
        Ok((gate, manager))
    }

    fn recover_switch(manager: &ManagerLock) -> Result<()> {
        match fs::symlink_metadata(manager.prefix.join(PENDING)) {
            Ok(_) => native_install::recover(manager),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn rollback_uncommitted(
        controller: &ControllerLock,
        executable: &Path,
        intent: &Intent,
        recover_stale: bool,
    ) -> Result<Intent> {
        let (gate, manager) =
            quiescent_manager(controller, executable, intent, recover_stale).await?;
        wait_inactive(controller.prefix(), intent).await?;
        recover_switch(&manager)?;
        let rollback = match intent.phase {
            Phase::Preparing | Phase::Trial => {
                service_update::select_trial(controller, &gate, &manager, intent, true)?
            }
            Phase::RollbackTrial => intent.clone(),
            _ => {
                return Err(Error::message(
                    "A committed service update cannot be rolled back automatically",
                ));
            }
        };
        native_install::verify_stored(controller.prefix(), &rollback.old_current)?;
        native_install::verify_service_protocol(controller.prefix(), &rollback.old_current)?;
        native_install::restore_service(&manager, &rollback)?;
        drop(manager);
        drop(gate);
        wait_inactive(controller.prefix(), &rollback).await?;
        start(&rollback.unit).await?;
        finish_started(controller, &rollback).await
    }

    async fn continue_committed(
        controller: &ControllerLock,
        executable: &Path,
        intent: &Intent,
        recover_stale: bool,
    ) -> Result<Intent> {
        let gate = barrier(controller.prefix()).await?;
        if let Ok(manager) = ManagerLock::for_service(controller, executable, false, recover_stale)
        {
            if pointer(controller.prefix(), "current")? != expected_version(intent)? {
                return Err(Error::message("The committed version pointer changed"));
            }
            native_install::verify_stored(controller.prefix(), expected_version(intent)?)?;
            native_install::verify_service_protocol(
                controller.prefix(),
                expected_version(intent)?,
            )?;
            drop(manager);
            drop(gate);
            wait_inactive(controller.prefix(), intent).await?;
            start(&intent.unit).await?;
        } else {
            drop(gate);
        }
        finish_started(controller, intent).await
    }

    pub async fn recover(prefix: &Path, executable: &Path, recover_stale: bool) -> Result<Intent> {
        let controller = ControllerLock::acquire(prefix, executable)?;
        let intent = service_update::read(prefix)?
            .ok_or_else(|| Error::message("No service update is pending"))?;
        match intent.phase {
            Phase::Preparing | Phase::Trial => {
                rollback_uncommitted(&controller, executable, &intent, recover_stale).await
            }
            Phase::RollbackTrial => {
                let gate = barrier(prefix).await?;
                let inactive =
                    ManagerLock::for_service(&controller, executable, true, recover_stale).is_ok();
                drop(gate);
                if inactive {
                    rollback_uncommitted(&controller, executable, &intent, recover_stale).await
                } else {
                    finish_started(&controller, &intent).await
                }
            }
            Phase::CommitTrial | Phase::CommitRollback => {
                continue_committed(&controller, executable, &intent, recover_stale).await
            }
            Phase::AcceptedTrial | Phase::AcceptedRollback => {
                let gate = barrier(prefix).await?;
                service_update::finish(&controller, &gate, &intent)?;
                Ok(intent)
            }
        }
        .map_err(pending_error)
    }

    pub struct Candidate<'a> {
        pub version: &'a str,
        pub binary: &'a [u8],
        pub sha256: &'a str,
    }

    pub async fn upgrade(
        prefix: &Path,
        executable: &Path,
        guard: RunningInstall,
        unit: &str,
        config: &Path,
        candidate: Candidate<'_>,
    ) -> Result<Intent> {
        let controller = ControllerLock::acquire(prefix, executable)?;
        let current = pointer(prefix, "current")?;
        native_install::verify_stored(prefix, &current)?;
        native_install::verify_service_protocol(prefix, &current)?;
        native_install::preflight_service(
            &controller,
            candidate.version,
            candidate.binary,
            candidate.sha256,
        )?;
        let gate = barrier(prefix).await?;
        let identity = systemd_identity::inspect(&guard, prefix, config, unit).await?;
        let snapshot = probe_profile(&guard, prefix, config).await?;
        let intent = service_update::begin(
            &controller,
            &gate,
            &guard,
            &snapshot,
            &identity,
            config,
            candidate.version,
        )?;
        match stop_exact_idle(&guard, prefix, config, &snapshot).await {
            Ok(_) => {}
            Err(error @ (Error::IdleBusy | Error::IdleIsolation)) => {
                let cancellation = async {
                    let identity = systemd_identity::inspect(&guard, prefix, config, unit).await?;
                    let snapshot = probe_profile(&guard, prefix, config).await?;
                    service_update::cancel_refused(
                        &controller,
                        &gate,
                        &guard,
                        &intent,
                        &snapshot,
                        &identity,
                    )
                }
                .await;
                return if cancellation.is_ok() {
                    Err(error)
                } else {
                    Err(pending_error(error))
                };
            }
            Err(error) => return Err(pending_error(error)),
        }
        drop(guard);
        let staged = match wait_inactive(prefix, &intent).await {
            Ok(()) => (|| -> Result<Intent> {
                let manager = ManagerLock::for_service(&controller, executable, false, false)?;
                native_install::stage(
                    &manager,
                    candidate.version,
                    candidate.binary,
                    candidate.sha256,
                )?;
                native_install::verify_stored(prefix, candidate.version)?;
                native_install::verify_service_protocol(prefix, candidate.version)?;
                let _profile_locks =
                    native_install::validate_profiles(&manager, candidate.version)?;
                let trial =
                    service_update::select_trial(&controller, &gate, &manager, &intent, false)?;
                native_install::switch(&manager, candidate.version)?;
                Ok(trial)
            })(),
            Err(error) => Err(error),
        };
        drop(gate);
        let outcome = match staged {
            Ok(trial) => match wait_inactive(prefix, &trial).await {
                Ok(()) => match start(unit).await {
                    Ok(()) => finish_started(&controller, &trial).await,
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        match outcome {
            Ok(accepted) => Ok(accepted),
            Err(error) => {
                let pending = service_update::read(prefix)?;
                if let Some(pending) = pending.filter(|value| {
                    matches!(
                        value.phase,
                        Phase::Preparing | Phase::Trial | Phase::RollbackTrial
                    )
                }) {
                    return match rollback_uncommitted(&controller, executable, &pending, false)
                        .await
                    {
                        Ok(_) => Err(Error::message(format!(
                            "{error}; the previous service version was restored and verified"
                        ))),
                        Err(_) => Err(pending_error(error)),
                    };
                }
                Err(pending_error(error))
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{Candidate, recover, upgrade};
