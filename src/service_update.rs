//! Persistent state and launch rules for a future coordinated service update.
//!
//! This module does not stop or start a service.  In particular, the presence
//! of a record does not mean an update has been accepted.  Trial processes may
//! connect to the Hub but cannot execute requests until their exact instance is
//! named in a durable commit record and explicitly activated.

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, File};
    use std::path::{Component, Path, PathBuf};

    use serde::{Deserialize, Serialize};

    use crate::error::{Error, Result};
    use crate::install_lock::{
        ControllerLock, LaunchBarrier, ManagerLock, RunningInstall, RunningRecord, pointer,
    };
    use crate::readiness::Snapshot;
    use crate::systemd_identity::UnitIdentity;
    use crate::{storage, update};

    pub const RECORD: &str = ".service-update.json";

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Phase {
        Preparing,
        Trial,
        CommitTrial,
        AcceptedTrial,
        RollbackTrial,
        CommitRollback,
        AcceptedRollback,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Proof {
        pub pid: u32,
        pub nonce: String,
        pub instance_id: String,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct Intent {
        pub schema: u8,
        pub transaction_id: String,
        pub unit: String,
        pub config: String,
        pub old_current: String,
        pub old_previous: Option<String>,
        pub new_current: String,
        pub original: Proof,
        pub original_invocation: String,
        pub original_definition: String,
        pub phase: Phase,
        pub proof: Option<Proof>,
    }

    fn hex_id(value: &str) -> bool {
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn digest(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    impl Intent {
        pub fn validate(&self) -> Result<()> {
            let config = Path::new(&self.config);
            let proof_required = matches!(
                self.phase,
                Phase::CommitTrial
                    | Phase::AcceptedTrial
                    | Phase::CommitRollback
                    | Phase::AcceptedRollback
            );
            let valid_proof = self.proof.as_ref().is_some_and(|proof| {
                proof.pid > 0
                    && proof.pid <= i32::MAX as u32
                    && hex_id(&proof.nonce)
                    && hex_id(&proof.instance_id)
            });
            if self.schema != 1
                || !hex_id(&self.transaction_id)
                || !crate::systemd_identity::valid_unit(&self.unit)
                || self.config.len() > 4096
                || self.config.chars().any(char::is_control)
                || !config.is_absolute()
                || !config
                    .components()
                    .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
                || !update::valid_version(&self.old_current)
                || !update::valid_version(&self.new_current)
                || self.old_current == self.new_current
                || self.original.pid == 0
                || self.original.pid > i32::MAX as u32
                || !hex_id(&self.original.nonce)
                || !hex_id(&self.original.instance_id)
                || !hex_id(&self.original_invocation)
                || !digest(&self.original_definition)
                || self
                    .old_previous
                    .as_deref()
                    .is_some_and(|version| !update::valid_version(version))
                || proof_required != self.proof.is_some()
                || (proof_required && !valid_proof)
            {
                return Err(Error::message("Invalid service update record"));
            }
            Ok(())
        }

        fn selection(&self) -> Result<(&str, bool)> {
            match self.phase {
                Phase::Preparing => Err(Error::message(
                    "A service update was interrupted before its trial; inspect and recover it before starting",
                )),
                Phase::Trial | Phase::CommitTrial => Ok((&self.new_current, true)),
                Phase::AcceptedTrial => Ok((&self.new_current, false)),
                Phase::RollbackTrial | Phase::CommitRollback => Ok((&self.old_current, true)),
                Phase::AcceptedRollback => Ok((&self.old_current, false)),
            }
        }
    }

    pub fn read(prefix: &Path) -> Result<Option<Intent>> {
        let path = prefix.join(RECORD);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let bytes = storage::read_private(&path)?;
        if bytes.len() > 8192 {
            return Err(Error::message("Invalid service update record"));
        }
        let intent: Intent = serde_json::from_slice(&bytes)
            .map_err(|_| Error::message("Invalid service update record"))?;
        intent.validate()?;
        Ok(Some(intent))
    }

    fn same_installation(controller: &ControllerLock, barrier: &LaunchBarrier) -> Result<()> {
        if controller.prefix() != barrier.prefix() {
            return Err(Error::message(
                "Service update locks belong to different installations",
            ));
        }
        Ok(())
    }

    fn live_proof(
        controller: &ControllerLock,
        guard: &RunningInstall,
        snapshot: &Snapshot,
        identity: &UnitIdentity,
        config: &Path,
        unit: &str,
    ) -> Result<Proof> {
        if controller.prefix() != guard.prefix()
            || !snapshot.ready
            || identity.transient
            || identity.unit != unit
            || identity.pid != snapshot.pid
            || identity.version != snapshot.version
            || identity.instance_id != snapshot.instance_id
            || !identity.ready
            || identity.connection_generation != snapshot.connection_generation
            || identity.quarantined != snapshot.quarantined
            || Path::new(&snapshot.config).canonicalize().ok().as_deref() != Some(config)
            || identity.executable != controller.prefix().join("current/rdh").to_string_lossy()
        {
            return Err(Error::message("Service update identity evidence changed"));
        }
        let records = guard.running_records()?;
        if records.len() != 1
            || records[0].pid != snapshot.pid
            || records[0].config.canonicalize().ok().as_deref() != Some(config)
        {
            return Err(Error::message(
                "Service update requires exactly one matching running Agent",
            ));
        }
        let proof = Proof {
            pid: snapshot.pid,
            nonce: records[0].nonce.clone(),
            instance_id: snapshot.instance_id.clone(),
        };
        if !hex_id(&proof.nonce)
            || !hex_id(&proof.instance_id)
            || !hex_id(&identity.invocation_id)
            || !digest(&identity.definition_hash)
        {
            return Err(Error::message("Invalid service update identity evidence"));
        }
        Ok(proof)
    }

    /// Write the durable intent before requesting the old Agent to stop. The
    /// exclusive launch barrier ensures there is no unpublished Agent launch.
    pub fn begin(
        controller: &ControllerLock,
        barrier: &LaunchBarrier,
        guard: &RunningInstall,
        snapshot: &Snapshot,
        identity: &UnitIdentity,
        config: &Path,
        target: &str,
    ) -> Result<Intent> {
        same_installation(controller, barrier)?;
        if read(controller.prefix())?.is_some() {
            return Err(Error::message("A service update is already pending"));
        }
        let config = config.canonicalize()?;
        let current = pointer(controller.prefix(), "current")?;
        if snapshot.quarantined || snapshot.version != current {
            return Err(Error::message("The original service Agent is not active"));
        }
        let original = live_proof(
            controller,
            guard,
            snapshot,
            identity,
            &config,
            &identity.unit,
        )?;
        let intent = Intent {
            schema: 1,
            transaction_id: crate::random::hex_token::<16>()?,
            unit: identity.unit.clone(),
            config: config
                .to_str()
                .ok_or_else(|| Error::message("Config path is not Unicode"))?
                .into(),
            old_current: current,
            old_previous: crate::native_install::previous(controller.prefix())?,
            new_current: target.into(),
            original,
            original_invocation: identity.invocation_id.clone(),
            original_definition: identity.definition_hash.clone(),
            phase: Phase::Preparing,
            proof: None,
        };
        intent.validate()?;
        storage::write_private_new(
            &controller.prefix().join(RECORD),
            &serde_json::to_vec(&intent)?,
        )?;
        Ok(intent)
    }

    fn replace(controller: &ControllerLock, expected: &Intent, next: &Intent) -> Result<()> {
        expected.validate()?;
        next.validate()?;
        if read(controller.prefix())?.as_ref() != Some(expected)
            || expected.transaction_id != next.transaction_id
        {
            return Err(Error::message("Service update record changed"));
        }
        storage::write_private_replace(
            &controller.prefix().join(RECORD),
            &serde_json::to_vec(next)?,
        )
    }

    /// Change which version may start only while no Agent owns the exclusive
    /// installation lock. Rollback is forbidden after a commit decision.
    pub fn select_trial(
        controller: &ControllerLock,
        barrier: &LaunchBarrier,
        manager: &ManagerLock,
        expected: &Intent,
        rollback: bool,
    ) -> Result<Intent> {
        same_installation(controller, barrier)?;
        if manager.prefix != controller.prefix()
            || !manager.owns_service(&expected.transaction_id)
            || !matches!(
                (expected.phase, rollback),
                (Phase::Preparing, false) | (Phase::Preparing, true) | (Phase::Trial, true)
            )
        {
            return Err(Error::message("Invalid service trial transition"));
        }
        let mut next = expected.clone();
        next.phase = if rollback {
            Phase::RollbackTrial
        } else {
            Phase::Trial
        };
        next.proof = None;
        replace(controller, expected, &next)?;
        Ok(next)
    }

    /// Commit only a ready quarantined instance. Once written, recovery rolls
    /// forward even if the activation acknowledgement is lost.
    pub fn commit(
        controller: &ControllerLock,
        barrier: &LaunchBarrier,
        guard: &RunningInstall,
        expected: &Intent,
        snapshot: &Snapshot,
        identity: &UnitIdentity,
    ) -> Result<Intent> {
        same_installation(controller, barrier)?;
        let (phase, selected) = match expected.phase {
            Phase::Trial | Phase::CommitTrial => (Phase::CommitTrial, &expected.new_current),
            Phase::RollbackTrial | Phase::CommitRollback => {
                (Phase::CommitRollback, &expected.old_current)
            }
            _ => return Err(Error::message("Invalid service commit transition")),
        };
        if !snapshot.quarantined
            || &snapshot.version != selected
            || pointer(controller.prefix(), "current")? != *selected
            || identity.definition_hash != expected.original_definition
        {
            return Err(Error::message(
                "Only the selected quarantined trial may be committed",
            ));
        }
        let config = Path::new(&expected.config);
        let proof = live_proof(
            controller,
            guard,
            snapshot,
            identity,
            config,
            &expected.unit,
        )?;
        let mut next = expected.clone();
        next.phase = phase;
        next.proof = Some(proof);
        replace(controller, expected, &next)?;
        Ok(next)
    }

    /// Finalize only the same live instance that the commit named. A separate
    /// accepted state makes a crash before journal removal safe to recover.
    pub fn accept(
        controller: &ControllerLock,
        barrier: &LaunchBarrier,
        guard: &RunningInstall,
        expected: &Intent,
        snapshot: &Snapshot,
        identity: &UnitIdentity,
    ) -> Result<Intent> {
        same_installation(controller, barrier)?;
        let (phase, selected) = match expected.phase {
            Phase::CommitTrial => (Phase::AcceptedTrial, &expected.new_current),
            Phase::CommitRollback => (Phase::AcceptedRollback, &expected.old_current),
            _ => return Err(Error::message("Invalid service acceptance transition")),
        };
        if snapshot.quarantined
            || &snapshot.version != selected
            || pointer(controller.prefix(), "current")? != *selected
            || identity.definition_hash != expected.original_definition
        {
            return Err(Error::message(
                "The committed service instance is not active",
            ));
        }
        let proof = live_proof(
            controller,
            guard,
            snapshot,
            identity,
            Path::new(&expected.config),
            &expected.unit,
        )?;
        if expected.proof.as_ref() != Some(&proof) {
            return Err(Error::message(
                "The active service instance differs from the committed trial",
            ));
        }
        let mut next = expected.clone();
        next.phase = phase;
        replace(controller, expected, &next)?;
        Ok(next)
    }

    pub fn finish(
        controller: &ControllerLock,
        barrier: &LaunchBarrier,
        expected: &Intent,
    ) -> Result<()> {
        same_installation(controller, barrier)?;
        let selected = match expected.phase {
            Phase::AcceptedTrial => &expected.new_current,
            Phase::AcceptedRollback => &expected.old_current,
            _ => return Err(Error::message("Only an accepted service update can finish")),
        };
        if pointer(controller.prefix(), "current")? != *selected
            || read(controller.prefix())?.as_ref() != Some(expected)
        {
            return Err(Error::message("Accepted service update state changed"));
        }
        fs::remove_file(controller.prefix().join(RECORD))?;
        File::open(controller.prefix())?.sync_all()?;
        Ok(())
    }

    /// Cancel only after the old Agent explicitly refused the stop and its
    /// original service invocation is still verified. Timeouts must not use
    /// this path because the request may already have been accepted.
    pub fn cancel_refused(
        controller: &ControllerLock,
        barrier: &LaunchBarrier,
        guard: &RunningInstall,
        expected: &Intent,
        snapshot: &Snapshot,
        identity: &UnitIdentity,
    ) -> Result<()> {
        same_installation(controller, barrier)?;
        if expected.phase != Phase::Preparing
            || snapshot.quarantined
            || snapshot.version != expected.old_current
            || identity.invocation_id != expected.original_invocation
            || identity.definition_hash != expected.original_definition
            || pointer(controller.prefix(), "current")? != expected.old_current
            || read(controller.prefix())?.as_ref() != Some(expected)
            || live_proof(
                controller,
                guard,
                snapshot,
                identity,
                Path::new(&expected.config),
                &expected.unit,
            )? != expected.original
        {
            return Err(Error::message(
                "The refused stop cannot be safely cancelled",
            ));
        }
        fs::remove_file(controller.prefix().join(RECORD))?;
        File::open(controller.prefix())?.sync_all()?;
        Ok(())
    }

    /// Information pinned while the launch gate is held. A trial process must
    /// keep admission closed even if the file subsequently changes.
    #[derive(Clone, Debug)]
    pub struct Launch {
        pub transaction_id: String,
        pub prefix: PathBuf,
        pub config: PathBuf,
        pub trial: bool,
    }

    pub fn launch(prefix: &Path, current: &str) -> Result<Option<Launch>> {
        let Some(intent) = read(prefix)? else {
            return Ok(None);
        };
        let (selected, trial) = intent.selection()?;
        if selected != current {
            return Err(Error::message(
                "The current native version does not match the service update record",
            ));
        }
        Ok(Some(Launch {
            transaction_id: intent.transaction_id,
            prefix: prefix.to_path_buf(),
            config: intent.config.into(),
            trial,
        }))
    }

    impl Launch {
        pub fn check_profile(&self, config: &Path, pair_only: bool) -> Result<()> {
            if pair_only || config.canonicalize().ok().as_deref() != Some(self.config.as_path()) {
                return Err(Error::message(
                    "Only the recorded existing service profile may start during a service update",
                ));
            }
            Ok(())
        }

        /// A durable commit is bound to one already-observed Agent instance.
        /// Merely being in a trial or accepted state is not an activation proof.
        pub fn committed_for(&self, record: &RunningRecord, instance_id: &str) -> Result<bool> {
            let Some(intent) = read(&self.prefix)? else {
                return Ok(false);
            };
            let Some(proof) = &intent.proof else {
                return Ok(false);
            };
            let selected = match intent.phase {
                Phase::CommitTrial => &intent.new_current,
                Phase::CommitRollback => &intent.old_current,
                _ => return Ok(false),
            };
            Ok(self.trial
                && intent.transaction_id == self.transaction_id
                && Path::new(&intent.config) == self.config
                && record.config.canonicalize().ok().as_deref() == Some(self.config.as_path())
                && selected == env!("CARGO_PKG_VERSION")
                && proof.pid == record.pid
                && proof.nonce == record.nonce
                && proof.instance_id == instance_id)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn intent() -> Intent {
            Intent {
                schema: 1,
                transaction_id: "a".repeat(32),
                unit: "rdh.service".into(),
                config: "/tmp/device.json".into(),
                old_current: "0.7.2".into(),
                old_previous: None,
                new_current: "0.7.3".into(),
                original: Proof {
                    pid: 1,
                    nonce: "d".repeat(32),
                    instance_id: "e".repeat(32),
                },
                original_invocation: "f".repeat(32),
                original_definition: "1".repeat(64),
                phase: Phase::Preparing,
                proof: None,
            }
        }

        #[test]
        fn phase_selects_one_version_and_commits_require_an_exact_identity() {
            let mut value = intent();
            assert!(value.validate().is_ok());
            assert!(value.selection().is_err());
            value.phase = Phase::Trial;
            assert_eq!(value.selection().unwrap(), ("0.7.3", true));
            value.phase = Phase::CommitTrial;
            assert!(value.validate().is_err());
            value.proof = Some(Proof {
                pid: 1,
                nonce: "b".repeat(32),
                instance_id: "c".repeat(32),
            });
            assert!(value.validate().is_ok());
            value.phase = Phase::AcceptedTrial;
            assert_eq!(value.selection().unwrap(), ("0.7.3", false));
            value.phase = Phase::RollbackTrial;
            value.proof = None;
            assert_eq!(value.selection().unwrap(), ("0.7.2", true));
            value.config = "/tmp/../other".into();
            assert!(value.validate().is_err());
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    Intent, Launch, Phase, Proof, RECORD, accept, begin, cancel_refused, commit, finish, launch,
    read, select_trial,
};
