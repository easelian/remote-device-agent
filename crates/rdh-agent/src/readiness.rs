//! Local evidence of the current managed Agent connection and a guarded stop.
//! A live Unix peer, its process identity, and the installation marker must all
//! agree; a persistent marker or an old log line alone is never readiness.

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{Semaphore, watch};
    use tokio::task::JoinHandle;

    use crate::config::DeviceConfig;
    use crate::error::{Error, Result};
    use crate::install_lock::{RunningInstall, RunningRecord, pointer, version_binary};
    use crate::rpc::Runtime;
    use crate::service_update::Launch;

    const LIMIT: u64 = 32 * 1024;
    const DEADLINE: Duration = Duration::from_secs(2);

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        schema: u8,
        pid: u32,
        nonce: String,
        config: String,
        version: String,
        device_id: String,
        instance_id: String,
        connection_generation: Option<u64>,
        quarantined: bool,
    }

    #[derive(Clone, Debug, Serialize)]
    pub struct Snapshot {
        pub ready: bool,
        pub pid: u32,
        pub config: String,
        pub version: String,
        pub device_id: String,
        pub instance_id: String,
        pub connection_generation: Option<u64>,
        pub quarantined: bool,
    }

    fn address(record: &RunningRecord, operation: &str) -> Result<tokio::net::unix::SocketAddr> {
        // The nonce was generated for this marker and is kept in private
        // storage. Abstract sockets disappear when the process exits and do
        // not impose a filesystem path-length limit.
        let name = format!("rdh-{operation}-{}-{}", record.pid, record.nonce);
        Ok(std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?.into())
    }

    fn same_user(stream: &UnixStream) -> bool {
        stream.peer_cred().is_ok_and(|credentials| {
            // SAFETY: geteuid has no preconditions and does not mutate memory.
            credentials.uid() == unsafe { libc::geteuid() }
        })
    }

    pub(crate) fn process_handle(pid: u32) -> Result<std::fs::File> {
        // SAFETY: pid was validated as a positive pid_t. A successful syscall
        // returns a new owned descriptor; no pointers are passed to the kernel.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd as i32) })
    }
    fn process_exited(process: &std::fs::File) -> Result<bool> {
        let mut poll = libc::pollfd {
            fd: process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // A pidfd becomes readable when that exact process exits, even if its
        // numeric PID is later reused. Any poll failure is not proof of life.
        let status = unsafe { libc::poll(&mut poll, 1, 0) };
        if status < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if poll.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(true);
        }
        if status != 0 || poll.revents != 0 {
            return Err(Error::message("Cannot inspect Agent process"));
        }
        Ok(false)
    }
    pub(crate) fn require_live(process: &std::fs::File) -> Result<()> {
        if process_exited(process)? {
            return Err(Error::message(
                "Agent process exited during readiness probe",
            ));
        }
        Ok(())
    }

    pub struct Reporter {
        generation: Arc<AtomicU64>,
        task: Option<JoinHandle<()>>,
    }

    impl Reporter {
        pub fn start(
            record: RunningRecord,
            device_id: &str,
            instance_id: &str,
            runtime: Arc<Runtime>,
            shutdown: watch::Sender<bool>,
            trial: Option<Launch>,
        ) -> Result<Self> {
            Self::start_with_check(
                record,
                device_id,
                instance_id,
                runtime,
                shutdown,
                trial,
                || crate::cgroup::only_current_process().unwrap_or(false),
            )
        }

        fn start_with_check<F>(
            record: RunningRecord,
            device_id: &str,
            instance_id: &str,
            runtime: Arc<Runtime>,
            shutdown: watch::Sender<bool>,
            trial: Option<Launch>,
            check: F,
        ) -> Result<Self>
        where
            F: Fn() -> bool + Send + Sync + 'static,
        {
            if record.pid != std::process::id() {
                return Err(Error::message(
                    "Readiness marker belongs to another process",
                ));
            }
            let config = record
                .config
                .to_str()
                .ok_or_else(|| Error::message("Readiness config path is not Unicode"))?
                .to_owned();
            let listener = UnixListener::bind_addr(&address(&record, "ready")?)?;
            let control = UnixListener::bind_addr(&address(&record, "idle")?)?;
            let activation = UnixListener::bind_addr(&address(&record, "activate")?)?;
            let trial_stop = UnixListener::bind_addr(&address(&record, "stoptrial")?)?;
            let generation = Arc::new(AtomicU64::new(0));
            let current = Arc::clone(&generation);
            let device_id = device_id.to_owned();
            let instance_id = instance_id.to_owned();
            let check = Arc::new(check);
            let inspections = Arc::new(Semaphore::new(1));
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, action) = tokio::select! {
                        accepted = listener.accept() => match accepted { Ok((stream,_)) => (stream,0), Err(_) => break },
                        accepted = control.accept() => match accepted { Ok((stream,_)) => (stream,1), Err(_) => break },
                        accepted = activation.accept() => match accepted { Ok((stream,_)) => (stream,2), Err(_) => break },
                        accepted = trial_stop.accept() => match accepted { Ok((stream,_)) => (stream,3), Err(_) => break },
                    };
                    if !same_user(&stream) {
                        continue;
                    }
                    if action != 0 {
                        let mut requested = [0u8; 32];
                        if !matches!(
                            tokio::time::timeout(DEADLINE, stream.read_exact(&mut requested)).await,
                            Ok(Ok(_))
                        ) || requested != instance_id.as_bytes()
                        {
                            let _ = tokio::time::timeout(DEADLINE, stream.write_all(b"I")).await;
                            continue;
                        }
                        if action == 2 {
                            let committed = current.load(Ordering::Acquire) != 0
                                && trial.as_ref().is_some_and(|trial| {
                                    trial.committed_for(&record, &instance_id).unwrap_or(false)
                                });
                            if committed {
                                runtime.activate_trial();
                            }
                            let response = if committed { b"A" } else { b"I" };
                            let _ =
                                tokio::time::timeout(DEADLINE, stream.write_all(response)).await;
                            continue;
                        }
                        if action == 3 && (trial.is_none() || !runtime.is_quarantined()) {
                            let _ = tokio::time::timeout(DEADLINE, stream.write_all(b"I")).await;
                            continue;
                        }
                        let frozen = if action == 3 {
                            runtime.try_freeze_trial()
                        } else {
                            runtime.try_freeze_idle()
                        };
                        let Some(frozen) = frozen else {
                            let _ = tokio::time::timeout(DEADLINE, stream.write_all(b"B")).await;
                            continue;
                        };
                        let Ok(inspection) = Arc::clone(&inspections).try_acquire_owned() else {
                            let _ = tokio::time::timeout(DEADLINE, stream.write_all(b"G")).await;
                            drop(frozen);
                            continue;
                        };
                        let inspect = Arc::clone(&check);
                        let isolated = matches!(
                            tokio::time::timeout(
                                DEADLINE,
                                tokio::task::spawn_blocking(move || {
                                    let _inspection = inspection;
                                    inspect()
                                })
                            )
                            .await,
                            Ok(Ok(true))
                        );
                        if !isolated {
                            let _ = tokio::time::timeout(DEADLINE, stream.write_all(b"G")).await;
                            drop(frozen);
                            continue;
                        }
                        if matches!(
                            tokio::time::timeout(DEADLINE, stream.write_all(b"A")).await,
                            Ok(Ok(()))
                        ) {
                            runtime.begin_shutdown();
                            let _ = shutdown.send(true);
                            // Keep every admission permit until the session has
                            // ended and the app aborts this reporter task.
                            let _frozen = frozen;
                            std::future::pending::<()>().await;
                            return;
                        }
                        drop(frozen);
                        continue;
                    }
                    let active = current.load(Ordering::Acquire);
                    let wire = Wire {
                        schema: 1,
                        pid: record.pid,
                        nonce: record.nonce.clone(),
                        config: config.clone(),
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                        device_id: device_id.clone(),
                        instance_id: instance_id.clone(),
                        connection_generation: (active != 0).then_some(active),
                        quarantined: runtime.is_quarantined(),
                    };
                    let Ok(bytes) = serde_json::to_vec(&wire) else {
                        continue;
                    };
                    if bytes.len() as u64 <= LIMIT {
                        let _ = tokio::time::timeout(DEADLINE, stream.write_all(&bytes)).await;
                    }
                }
            });
            Ok(Self {
                generation,
                task: Some(task),
            })
        }

        pub fn ready(&self, generation: u64) {
            if generation != 0 {
                self.generation.store(generation, Ordering::Release);
            }
        }
        pub fn offline(&self) {
            self.generation.store(0, Ordering::Release);
        }
        pub async fn close(mut self) {
            self.offline();
            if let Some(task) = self.task.take() {
                task.abort();
                let _ = task.await;
            }
        }
    }
    impl Drop for Reporter {
        fn drop(&mut self) {
            self.offline();
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    struct Peer {
        record: RunningRecord,
        selected: String,
        device_id: String,
        process: std::fs::File,
        stream: UnixStream,
    }

    async fn connect_profile(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        operation: &str,
    ) -> Result<Peer> {
        if !config.is_absolute() {
            return Err(Error::message("Readiness profile path must be absolute"));
        }
        let config = config.canonicalize()?;
        let mut matches = guard.running_records()?.into_iter().filter(|record| {
            record
                .config
                .canonicalize()
                .is_ok_and(|path| path == config)
        });
        let record = matches
            .next()
            .ok_or_else(|| Error::message("No running Agent for this profile"))?;
        if matches.next().is_some() {
            return Err(Error::message(
                "Multiple native running markers match this profile",
            ));
        }
        let selected = pointer(prefix, "current")?;
        let expected = std::fs::metadata(version_binary(prefix, &selected)?)?;
        let configured = DeviceConfig::read(&record.config)?;
        let process = process_handle(record.pid)?;
        require_live(&process)?;
        let stream = tokio::time::timeout(
            DEADLINE,
            UnixStream::connect_addr(&address(&record, operation)?),
        )
        .await
        .map_err(|_| Error::message("Agent readiness connection timed out"))??;
        let peer = stream.peer_cred()?;
        // SAFETY: geteuid has no preconditions and does not mutate memory.
        if peer.uid() != unsafe { libc::geteuid() } || peer.pid() != Some(record.pid as i32) {
            return Err(Error::message(
                "Readiness peer does not match the running Agent",
            ));
        }
        let actual = std::fs::metadata(format!("/proc/{}/exe", record.pid))?;
        if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
            return Err(Error::message(
                "Running Agent does not match the current native binary",
            ));
        }
        Ok(Peer {
            record,
            selected,
            device_id: configured.device_id,
            process,
            stream,
        })
    }

    /// Probe a single profile while holding a shared installation lock. The
    /// response is a point-in-time view of the Agent's connection state.
    pub async fn probe_profile(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
    ) -> Result<Snapshot> {
        let Peer {
            record,
            selected,
            device_id,
            process,
            stream,
        } = connect_profile(guard, prefix, config, "ready").await?;
        let mut bytes = Vec::new();
        tokio::time::timeout(DEADLINE, stream.take(LIMIT + 1).read_to_end(&mut bytes))
            .await
            .map_err(|_| Error::message("Agent readiness response timed out"))??;
        if bytes.len() as u64 > LIMIT {
            return Err(Error::message("Agent readiness response is too large"));
        }
        let wire: Wire = serde_json::from_slice(&bytes)?;
        if wire.schema != 1
            || wire.pid != record.pid
            || wire.nonce != record.nonce
            || Path::new(&wire.config) != record.config
            || wire.version != selected
            || wire.device_id != device_id
            || wire.instance_id.len() != 32
            || !wire
                .instance_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || wire.connection_generation == Some(0)
        {
            return Err(Error::message(
                "Agent readiness identity does not match this installation",
            ));
        }
        require_live(&process)?;
        Ok(Snapshot {
            ready: wire.connection_generation.is_some(),
            pid: wire.pid,
            config: wire.config,
            version: wire.version,
            device_id: wire.device_id,
            instance_id: wire.instance_id,
            connection_generation: wire.connection_generation,
            quarantined: wire.quarantined,
        })
    }

    /// Activate only an exact ready trial named by a durable commit. If the
    /// acknowledgement is lost, callers must inspect rather than roll back.
    pub async fn activate_profile(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        expected: &Snapshot,
    ) -> Result<Snapshot> {
        if !expected.ready || !expected.quarantined {
            return Err(Error::message("A ready service update trial is required"));
        }
        let Peer {
            record,
            selected,
            device_id,
            process,
            mut stream,
        } = connect_profile(guard, prefix, config, "activate").await?;
        if record.pid != expected.pid
            || selected != expected.version
            || device_id != expected.device_id
        {
            return Err(Error::message(
                "Service update trial changed before activation",
            ));
        }
        tokio::time::timeout(DEADLINE, stream.write_all(expected.instance_id.as_bytes()))
            .await
            .map_err(|_| Error::message("Trial activation outcome is unknown"))?
            .map_err(|_| Error::message("Trial activation outcome is unknown"))?;
        let mut response = [0u8; 1];
        tokio::time::timeout(DEADLINE * 2, stream.read_exact(&mut response))
            .await
            .map_err(|_| Error::message("Trial activation outcome is unknown"))?
            .map_err(|_| Error::message("Trial activation outcome is unknown"))?;
        if response[0] != b'A' {
            return Err(Error::message("Trial activation was not authorized"));
        }
        require_live(&process)?;
        let snapshot = probe_profile(guard, prefix, config).await?;
        if !snapshot.ready
            || snapshot.quarantined
            || snapshot.pid != expected.pid
            || snapshot.version != expected.version
            || snapshot.device_id != expected.device_id
            || snapshot.instance_id != expected.instance_id
        {
            return Err(Error::message(
                "Trial activation was acknowledged but its identity or readiness changed; inspect before continuing",
            ));
        }
        Ok(snapshot)
    }

    /// Ask the same ready instance to stop only if all work and its cgroup are
    /// idle. An accepted request is not replayed if its outcome is uncertain.
    pub async fn stop_if_idle(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
    ) -> Result<Snapshot> {
        let snapshot = probe_profile(guard, prefix, config).await?;
        stop_exact_idle(guard, prefix, config, &snapshot).await
    }

    /// Stop only the same instance that was inspected before the durable
    /// pre-stop intent was created.
    pub async fn stop_exact_idle(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        snapshot: &Snapshot,
    ) -> Result<Snapshot> {
        if !snapshot.ready || snapshot.quarantined {
            return Err(Error::message(
                "An offline Agent cannot be stopped through the idle update path",
            ));
        }
        stop_snapshot(guard, prefix, config, snapshot, "idle").await
    }

    /// An unactivated trial admits no work and may be shut down even if it
    /// never connected. The reporter rechecks quarantine before freezing it.
    pub async fn stop_trial(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        snapshot: &Snapshot,
    ) -> Result<Snapshot> {
        if !snapshot.quarantined {
            return Err(Error::message(
                "Only a quarantined trial may use the trial stop path",
            ));
        }
        stop_snapshot(guard, prefix, config, snapshot, "stoptrial").await
    }

    async fn stop_snapshot(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        snapshot: &Snapshot,
        operation: &str,
    ) -> Result<Snapshot> {
        let Peer {
            record,
            selected,
            device_id,
            process,
            mut stream,
        } = connect_profile(guard, prefix, config, operation).await?;
        if record.pid != snapshot.pid
            || selected != snapshot.version
            || device_id != snapshot.device_id
        {
            return Err(Error::message(
                "Agent instance changed before the idle request",
            ));
        }
        tokio::time::timeout(DEADLINE, stream.write_all(snapshot.instance_id.as_bytes()))
            .await
            .map_err(|_| Error::message("Idle request timed out; outcome is unknown"))?
            .map_err(|_| Error::message("Idle request failed; outcome is unknown"))?;
        let mut response = [0u8; 1];
        tokio::time::timeout(DEADLINE * 3, stream.read_exact(&mut response))
            .await
            .map_err(|_| Error::message("Idle request timed out; outcome is unknown"))?
            .map_err(|_| Error::message("Idle request failed; outcome is unknown"))?;
        match response[0] {
            b'B' => {
                return Err(Error::IdleBusy);
            }
            b'G' => {
                return Err(Error::IdleIsolation);
            }
            b'I' => {
                return Err(Error::message(
                    "Agent instance changed before the idle request",
                ));
            }
            b'A' => {}
            _ => return Err(Error::message("Idle request outcome is unknown")),
        }
        let exit = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if process_exited(&process)? {
                    return Ok::<(), Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        if !matches!(exit, Ok(Ok(()))) {
            return Err(Error::message(
                "Idle stop was accepted but process exit is not confirmed; inspect before retrying",
            ));
        }
        let canonical = config.canonicalize().map_err(|_| {
            Error::message("Idle stop was accepted but the configuration cannot be inspected")
        })?;
        let records = guard.running_records().map_err(|_| {
            Error::message("Idle stop was accepted but running markers cannot be inspected")
        })?;
        if records.iter().any(|item| {
            (item.pid == record.pid && item.nonce == record.nonce)
                || item
                    .config
                    .canonicalize()
                    .map_or(true, |path| path == canonical)
        }) {
            return Err(Error::message(
                "An Agent marker remains or a new Agent started; inspect before updating",
            ));
        }
        if record.config.canonicalize().ok().as_deref() != Some(canonical.as_path()) {
            return Err(Error::message(
                "Idle stop was accepted but the configuration path changed",
            ));
        }
        let mut lock = crate::profile_lock::ProfileLocks::acquire(&[canonical]).map_err(|_| Error::message(
            "Idle stop was accepted but the configuration lock remains; inspect before updating"))?;
        lock.release().map_err(|_| {
            Error::message(
                "Idle stop was accepted but the configuration lock could not be released",
            )
        })?;
        Ok(snapshot.clone())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::policy::Policy;
        use crate::transport;
        use serde_json::json;

        #[test]
        fn pidfd_detects_the_exact_process_exit() {
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let process = process_handle(child.id()).unwrap();
            assert!(require_live(&process).is_ok());
            child.kill().unwrap();
            child.wait().unwrap();
            assert!(require_live(&process).is_err());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn idle_control_releases_on_refusal_and_holds_admission_through_shutdown() {
            let root = std::env::temp_dir().join(format!(
                "rdh-idle-control-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            let config = DeviceConfig::parse(
                json!({"gateway_url":"http://127.0.0.1:9", "device_id":"fixture",
                "token":"t".repeat(32), "roots":{"work":root}}),
            )
            .unwrap();
            let runtime = Runtime::new(config, Policy::default(), None).unwrap();
            let (sender, mut frames, _) = transport::channel();
            assert!(runtime.bind(1, sender.clone()));
            let (stop, mut stopped) = watch::channel(false);
            let mut record = RunningRecord {
                pid: std::process::id(),
                config: root.join("device.json"),
                nonce: crate::random::hex_token::<16>().unwrap(),
            };
            let instance = runtime.receipts.instance_id.clone();
            let reporter = Reporter::start_with_check(
                record.clone(),
                "fixture",
                &instance,
                Arc::clone(&runtime),
                stop.clone(),
                None,
                || false,
            )
            .unwrap();
            let mut wrong = UnixStream::connect_addr(&address(&record, "idle").unwrap())
                .await
                .unwrap();
            wrong
                .write_all(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
                .await
                .unwrap();
            let mut response = [0u8; 1];
            wrong.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"I");
            let mut denied = UnixStream::connect_addr(&address(&record, "idle").unwrap())
                .await
                .unwrap();
            denied.write_all(instance.as_bytes()).await.unwrap();
            denied.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"G");
            assert!(!*stopped.borrow());
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(guard) = runtime.try_freeze_idle() {
                        drop(guard);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
            reporter.close().await;

            record.nonce = crate::random::hex_token::<16>().unwrap();
            let reporter = Reporter::start_with_check(
                record.clone(),
                "fixture",
                &instance,
                Arc::clone(&runtime),
                stop,
                None,
                || true,
            )
            .unwrap();
            let mut accepted = UnixStream::connect_addr(&address(&record, "idle").unwrap())
                .await
                .unwrap();
            accepted.write_all(instance.as_bytes()).await.unwrap();
            accepted.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"A");
            tokio::time::timeout(Duration::from_secs(3), stopped.changed())
                .await
                .unwrap()
                .unwrap();
            assert!(*stopped.borrow());
            let ids = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
            runtime
                .receive(
                    &json!({"id":"after-stop","operation":"write_file","params":{
                "root":"work","path":"must-not-exist","content":"x"}}),
                    &sender,
                    &ids,
                )
                .unwrap();
            let frame = tokio::time::timeout(Duration::from_secs(3), frames.recv())
                .await
                .unwrap()
                .unwrap();
            let value: serde_json::Value =
                serde_json::from_str(&frame.message.into_text().unwrap()).unwrap();
            assert_eq!(value["error_code"], "operation_busy");
            assert!(!root.join("must-not-exist").exists());
            reporter.close().await;
            assert!(runtime.shutdown().await);
            std::fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn timed_out_cgroup_inspection_cannot_spawn_unbounded_workers() {
            let root = std::env::temp_dir().join(format!(
                "rdh-idle-timeout-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            let config = DeviceConfig::parse(
                json!({"gateway_url":"http://127.0.0.1:9", "device_id":"fixture",
                "token":"t".repeat(32), "roots":{"work":root}}),
            )
            .unwrap();
            let runtime = Runtime::new(config, Policy::default(), None).unwrap();
            let (sender, _frames, _) = transport::channel();
            assert!(runtime.bind(1, sender));
            let (stop, _stopped) = watch::channel(false);
            let record = RunningRecord {
                pid: std::process::id(),
                config: root.join("device.json"),
                nonce: crate::random::hex_token::<16>().unwrap(),
            };
            let instance = runtime.receipts.instance_id.clone();
            let (release, wait_release) = std::sync::mpsc::channel::<()>();
            let wait_release = std::sync::Mutex::new(wait_release);
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let entered = Arc::clone(&count);
            let reporter = Reporter::start_with_check(
                record.clone(),
                "fixture",
                &instance,
                Arc::clone(&runtime),
                stop,
                None,
                move || {
                    entered.fetch_add(1, Ordering::AcqRel);
                    let _ = wait_release.lock().unwrap().recv();
                    false
                },
            )
            .unwrap();
            for _ in 0..2 {
                let mut control = UnixStream::connect_addr(&address(&record, "idle").unwrap())
                    .await
                    .unwrap();
                control.write_all(instance.as_bytes()).await.unwrap();
                let mut response = [0u8; 1];
                tokio::time::timeout(Duration::from_secs(5), control.read_exact(&mut response))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&response, b"G");
            }
            assert_eq!(count.load(Ordering::Acquire), 1);
            release.send(()).unwrap();
            reporter.close().await;
            assert!(runtime.shutdown().await);
            std::fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn offline_trial_can_stop_but_an_activated_instance_cannot_use_trial_stop() {
            let root = std::env::temp_dir().join(format!(
                "rdh-offline-trial-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            let config = DeviceConfig::parse(
                json!({"gateway_url":"http://127.0.0.1:9", "device_id":"fixture",
                "token":"t".repeat(32), "roots":{"work":root}}),
            )
            .unwrap();
            let runtime = Runtime::new(config, Policy::default(), None).unwrap();
            runtime.quarantine_before_connect();
            let (stop, mut stopped) = watch::channel(false);
            let record = RunningRecord {
                pid: std::process::id(),
                config: root.join("device.json"),
                nonce: crate::random::hex_token::<16>().unwrap(),
            };
            let instance = runtime.receipts.instance_id.clone();
            let trial = Launch {
                transaction_id: "a".repeat(32),
                prefix: root.clone(),
                config: record.config.clone(),
                trial: true,
            };
            let reporter = Reporter::start_with_check(
                record.clone(),
                "fixture",
                &instance,
                Arc::clone(&runtime),
                stop,
                Some(trial),
                || true,
            )
            .unwrap();
            runtime.activate_trial();
            let mut activated = UnixStream::connect_addr(&address(&record, "stoptrial").unwrap())
                .await
                .unwrap();
            activated.write_all(instance.as_bytes()).await.unwrap();
            let mut response = [0u8; 1];
            activated.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"I");
            assert!(!*stopped.borrow());
            runtime.quarantine_before_connect();
            let mut offline = UnixStream::connect_addr(&address(&record, "stoptrial").unwrap())
                .await
                .unwrap();
            offline.write_all(instance.as_bytes()).await.unwrap();
            offline.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"A");
            stopped.changed().await.unwrap();
            assert!(*stopped.borrow());
            reporter.close().await;
            assert!(runtime.shutdown().await);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    Reporter, Snapshot, activate_profile, probe_profile, stop_exact_idle, stop_if_idle, stop_trial,
};

#[cfg(target_os = "linux")]
pub(crate) use linux::{process_handle, require_live};

#[cfg(not(target_os = "linux"))]
pub struct Reporter;
#[cfg(not(target_os = "linux"))]
impl Reporter {
    pub fn ready(&self, _: u64) {}
    pub fn offline(&self) {}
    pub async fn close(self) {}
}
