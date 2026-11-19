use super::*;
use crate::{profile_lock::ProfileLocks, storage};
use std::{io::Write, os::unix::fs::PermissionsExt};

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    executable: PathBuf,
    expected_version: String,
    lock: Option<ProfileLocks>,
    evidence: fs::File,
}
impl Fixture {
    fn new(label: &str) -> Self {
        let destination = PathBuf::from(
            std::env::var_os("RDH_D2A_EVIDENCE").expect("explicit evidence directory required"),
        );
        let base = destination.join(format!(
            "fixture-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        storage::create_private_dir(&base).unwrap();
        let root = base.join(".rdh-browser");
        storage::create_private_dir(&root).unwrap();
        let lock = ProfileLocks::acquire(&[crate::browser::storage_lock(&root)]).unwrap();
        let evidence = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination.join(format!("{label}.jsonl")))
            .unwrap();
        Self {
            base,
            root,
            lock: Some(lock),
            evidence,
            executable: PathBuf::from(
                std::env::var_os("RDH_FIREFOX_EXECUTABLE")
                    .expect("explicit Firefox executable required"),
            ),
            expected_version: std::env::var("RDH_FIREFOX_VERSION")
                .expect("pinned Firefox version required"),
        }
    }
    fn record(&mut self, value: Value) {
        writeln!(self.evidence, "{value}").unwrap();
        self.evidence.flush().unwrap();
    }
    async fn open(&mut self) -> ManagedFirefox {
        let started = Instant::now();
        let (browser, product) = ManagedFirefox::open(&self.executable, &self.root)
            .await
            .unwrap();
        assert_eq!(product["version"], self.expected_version);
        assert_eq!(
            fs::metadata(&browser.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(browser.directory.join("owner.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        self.record(json!({"kind":"open","product":product,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"pid":browser.pid(),"private_directory":true,"owned_listener":true}));
        browser
    }
    fn finish(mut self) {
        super::super::recover(&self.root).unwrap();
        assert_eq!(fs::read_dir(&self.root).unwrap().count(), 0);
        self.lock.take().unwrap().release().unwrap();
        let mut lock = ProfileLocks::acquire(&[crate::browser::storage_lock(&self.root)]).unwrap();
        lock.release().unwrap();
        self.record(json!({"kind":"finished","directories_remaining":0,"lock_reacquired":true}));
        fs::remove_dir_all(&self.base).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.base.exists() && std::thread::panicking() {
            eprintln!(
                "D2a fixture retained for diagnosis: {}",
                self.base.display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned Firefox and an explicit private evidence directory"]
async fn native_start_session_events_and_close() {
    let mut fixture = Fixture::new("native-lifecycle");
    for iteration in 0..20 {
        let mut browser = fixture.open().await;
        assert!(ProfileLocks::acquire(&[crate::browser::storage_lock(&fixture.root)]).is_err());
        let deadline = Instant::now() + STARTUP;
        let client = browser.client.as_mut().unwrap();
        client.request(Command::Subscribe, deadline).await.unwrap();
        let result = client
            .request(Command::CreateContext, deadline)
            .await
            .unwrap();
        let context = result["context"].as_str().unwrap();
        client
            .wait_event("browsingContext.contextCreated", context, deadline)
            .await
            .unwrap();
        client
            .request(Command::CloseContext(context.into()), deadline)
            .await
            .unwrap();
        client
            .wait_event("browsingContext.contextDestroyed", context, deadline)
            .await
            .unwrap();
        let close = browser.close(true).await.unwrap();
        fixture.record(json!({"kind":"cycle","iteration":iteration,"created_event":true,"destroyed_event":true,"close":close}));
        assert_eq!(close["forced"], false);
        assert_eq!(close["processes_remaining"], 0);
        assert_eq!(close["directory_remaining"], false);
        assert!(members(browser.pid()).unwrap().is_empty());
    }
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned Firefox and an explicit private evidence directory"]
async fn startup_failure_cancellation_and_recovery_preserve_other_sessions() {
    let mut fixture = Fixture::new("startup-recovery");
    for executable in [
        Path::new("/rdh-d2a-missing-executable"),
        Path::new("/bin/true"),
    ] {
        let result = ManagedFirefox::open(executable, &fixture.root).await;
        assert!(result.is_err());
        fixture.record(json!({"kind":"invalid_executable","missing":!executable.exists(),"error":result.err().unwrap()}));
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    }
    let mut sentinel = fixture.open().await;
    for stage in ["before_endpoint", "after_endpoint", "after_handshake"] {
        for iteration in 0..5 {
            let mut browser = ManagedFirefox::spawn(&fixture.executable, &fixture.root).unwrap();
            let deadline = Instant::now() + STARTUP;
            if stage != "before_endpoint" {
                browser.discover(deadline).await.unwrap();
            }
            if stage == "after_handshake" {
                browser.connect(deadline).await.unwrap();
            }
            let pid = browser.pid();
            let directory = browser.directory.clone();
            // A live sentinel prevents namespace recovery. Neither live
            // session may be signalled by this read-only recovery check.
            assert!(super::super::recover(&fixture.root).is_err());
            // Simulate a cancelled owner future by dropping its process guard.
            drop(browser);
            assert!(members(pid).unwrap().is_empty());
            assert!(directory.exists());
            sentinel
                .client
                .as_mut()
                .unwrap()
                .request(Command::Status, Instant::now() + STARTUP)
                .await
                .unwrap();
            // Isolate the closed record for the existing recovery procedure.
            let recovery = fixture.base.join("recovery");
            storage::create_private_dir(&recovery).unwrap();
            fs::rename(&directory, recovery.join(directory.file_name().unwrap())).unwrap();
            super::super::recover(&recovery).unwrap();
            assert_eq!(fs::read_dir(&recovery).unwrap().count(), 0);
            fs::remove_dir(recovery).unwrap();
            fixture.record(json!({"kind":"cancelled_owner","stage":stage,"iteration":iteration,"processes_remaining":0,"recovered":true,"other_session_alive":true}));
        }
    }
    let mut stopped = fixture.open().await;
    assert_eq!(
        unsafe { libc::kill(-(stopped.pid() as i32), libc::SIGSTOP) },
        0
    );
    let forced_close = stopped.close(true).await.unwrap();
    fixture.record(json!({"kind":"stopped_browser_force_close","close":forced_close}));
    assert_eq!(forced_close["forced"], true);
    assert_eq!(forced_close["processes_remaining"], 0);
    assert_eq!(forced_close["directory_remaining"], false);
    sentinel
        .client
        .as_mut()
        .unwrap()
        .request(Command::Status, Instant::now() + STARTUP)
        .await
        .unwrap();
    let close = sentinel.close(true).await.unwrap();
    fixture.record(json!({"kind":"sentinel_close","close":close}));
    fixture.finish();
}

// This test is also a narrowly scoped child process. It prints only result
// categories and synthetic context counts, never an endpoint or page content.
#[tokio::test]
async fn endpoint_client_worker() {
    let Some(endpoint) = std::env::var("RDH_D2A_WORKER_ENDPOINT").ok() else {
        return;
    };
    let bytes = format!("WebDriver BiDi listening on {endpoint}\n");
    let endpoint = Endpoint::announcement(bytes.as_bytes()).unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut evidence = json!({"uid":unsafe { libc::getuid() },"connected":false,
        "network_namespace":fs::read_link("/proc/self/ns/net").unwrap().to_string_lossy()});
    if let Ok(mut client) = Client::connect(&endpoint, deadline).await {
        evidence["connected"] = true.into();
        let status = client.request(Command::Status, deadline).await;
        evidence["status_succeeded"] = status.is_ok().into();
        let new_session = std::env::var("RDH_D2A_WORKER_NEW_SESSION").as_deref() == Ok("1");
        if new_session {
            evidence["session_new_succeeded"] = client
                .request(Command::NewSession, deadline)
                .await
                .is_ok()
                .into();
        }
        let tree = client.request(Command::GetTree, deadline).await;
        evidence["tree_succeeded"] = tree.is_ok().into();
        if let Ok(tree) = tree {
            evidence["context_count"] = tree["contexts"].as_array().map(|v| v.len()).into();
        }
        if new_session && evidence["session_new_succeeded"] == true {
            evidence["session_end_succeeded"] = client
                .request(Command::EndSession, deadline)
                .await
                .is_ok()
                .into();
        }
    }
    if let Some(path) = std::env::var_os("RDH_D2A_WORKER_HIDDEN") {
        evidence["private_file_readable"] = fs::read(path).is_ok().into();
    }
    println!("D2A_CLIENT={evidence}");
}

struct PublicWorker(PathBuf);
impl PublicWorker {
    fn new() -> Self {
        // This scratch directory contains only a test executable. No browser
        // data, session IDs or endpoint files become readable by other users.
        let directory = std::env::temp_dir().join(format!(
            "rdh-d2a-client-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let binary = directory.join("client");
        fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        Self(directory)
    }
    fn command(
        &self,
        endpoint: &Endpoint,
        hidden: &Path,
        new_session: bool,
    ) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sudo");
        command
            .args(["-n", "-u", "nobody", "--", "/usr/bin/env"])
            .arg(format!("RDH_D2A_WORKER_ENDPOINT={}", endpoint.uri()))
            .arg(format!("RDH_D2A_WORKER_HIDDEN={}", hidden.display()))
            .arg(format!(
                "RDH_D2A_WORKER_NEW_SESSION={}",
                if new_session { "1" } else { "0" }
            ))
            .arg(self.0.join("client"))
            .args([
                "--exact",
                "browser::process::bidi_probe::tests::endpoint_client_worker",
                "--nocapture",
            ]);
        command
    }
}
impl Drop for PublicWorker {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn child_probe(
    mut command: tokio::process::Command,
    endpoint: &Endpoint,
    hidden: Option<&Path>,
) -> Value {
    command
        .env("RDH_D2A_WORKER_ENDPOINT", endpoint.uri())
        .kill_on_drop(true);
    if let Some(hidden) = hidden {
        command.env("RDH_D2A_WORKER_HIDDEN", hidden);
    }
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "client probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("D2A_CLIENT="))
        .expect("worker evidence missing");
    serde_json::from_str(line).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned Firefox, bubblewrap and sudo -n -u nobody for a bounded local test"]
async fn local_endpoint_isolation_matrix() {
    use crate::{config::DeviceConfig, policy::Policy, sandbox::Sandbox};
    let mut fixture = Fixture::new("local-isolation");
    let mut browser = fixture.open().await;
    let endpoint = browser.endpoint.as_ref().unwrap();
    let executable = std::env::current_exe().unwrap();
    let worker = "browser::process::bidi_probe::tests::endpoint_client_worker";
    let hidden = browser.directory.join("synthetic-secret");
    storage::write_private_new(&hidden, b"D2a-synthetic-private-marker").unwrap();

    let mut direct = tokio::process::Command::new(&executable);
    direct.args(["--exact", worker, "--nocapture"]);
    let result = child_probe(direct, endpoint, Some(&hidden)).await;
    fixture
        .record(json!({"kind":"endpoint_access","subject":"same_uid_unsandboxed","result":result}));
    assert_eq!(result["connected"], true);

    let public_worker = PublicWorker::new();
    let result = child_probe(
        public_worker.command(endpoint, &hidden, false),
        endpoint,
        None,
    )
    .await;
    fixture.record(json!({"kind":"endpoint_access","subject":"other_uid_nobody","result":result}));
    assert_eq!(result["uid"], 65534);
    assert_eq!(result["private_file_readable"], false);
    drop(public_worker);

    let work = fixture.base.join("work");
    storage::create_private_dir(&work).unwrap();
    let config_file = fixture.base.join("device.json");
    storage::write_private_new(&config_file, b"synthetic-device-config").unwrap();
    storage::write_private_new(
        &fixture.base.join("device.policy.json"),
        b"synthetic-policy",
    )
    .unwrap();
    let config = DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8111","device_id":"d2a-fixture","token":"x".repeat(32),"roots":{"work":work},"allow_shell":true,"allow_write":true})).unwrap();
    for network in [true, false] {
        let policy = Policy::parse(
            &json!({"version":1,"sandbox":{"mode":"bwrap","network":network,"hide_home":false}}),
            &config,
        )
        .unwrap();
        let sandbox = Sandbox::prepare(&config, &policy, Some(&config_file)).unwrap();
        let shell = format!(
            "exec '{}' --exact {worker} --nocapture",
            executable.display()
        );
        let (program, arguments) = sandbox.command(&work, &shell).unwrap();
        let mut command = tokio::process::Command::new(program);
        command.args(arguments);
        let result = child_probe(command, endpoint, Some(&hidden)).await;
        fixture.record(json!({"kind":"endpoint_access","subject":"actual_agent_sandbox","network_allowed":network,"sandbox":sandbox.report(),"result":result}));
        assert_eq!(result["private_file_readable"], false);
        assert_eq!(result["connected"], network);
    }
    // Exposure measurements are not permission to publish this backend.
    let close = browser.close(true).await.unwrap();
    fixture.record(json!({"kind":"close","close":close,"public_backend_enabled":false}));
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measures an untrusted local user during the Firefox handshake window"]
async fn another_os_user_can_win_the_first_session() {
    let mut fixture = Fixture::new("first-session-isolation");
    let public_worker = PublicWorker::new();
    for iteration in 0..3 {
        let mut browser = ManagedFirefox::spawn(&fixture.executable, &fixture.root).unwrap();
        browser.discover(Instant::now() + STARTUP).await.unwrap();
        let hidden = browser.directory.join("synthetic-secret");
        storage::write_private_new(&hidden, b"D2a-synthetic-private-marker").unwrap();
        let endpoint = browser.endpoint.as_ref().unwrap();
        let result = child_probe(
            public_worker.command(endpoint, &hidden, true),
            endpoint,
            None,
        )
        .await;
        fixture.record(json!({"kind":"first_session_access","iteration":iteration,"subject":"other_uid_nobody","result":result}));
        assert_eq!(result["uid"], 65534);
        assert_eq!(result["private_file_readable"], false);
        // This assertion records a release-blocking limitation of the pinned
        // Firefox configuration; it is not an isolation acceptance pass.
        assert_eq!(result["session_new_succeeded"], true);
        assert_eq!(result["tree_succeeded"], true);
        let close = browser.close(false).await.unwrap();
        fixture.record(json!({"kind":"close","close":close,"product_isolation_passed":false}));
    }
    fixture.finish();
}

#[tokio::test]
async fn owner_crash_worker() {
    let Some(root) = std::env::var_os("RDH_D2A_CRASH_ROOT").map(PathBuf::from) else {
        return;
    };
    let executable = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let stage = std::env::var("RDH_D2A_CRASH_STAGE").unwrap();
    let mut browser = ManagedFirefox::spawn(&executable, &root).unwrap();
    if stage != "before_endpoint" {
        browser.discover(Instant::now() + STARTUP).await.unwrap();
    }
    if stage == "after_handshake" {
        browser.connect(Instant::now() + STARTUP).await.unwrap();
    }
    let marker = PathBuf::from(std::env::var_os("RDH_D2A_CRASH_MARKER").unwrap());
    storage::write_private_new(&marker, &serde_json::to_vec(&json!({"pid":browser.pid(),"owner":browser.owner,"directory":browser.directory.file_name().unwrap().to_str().unwrap()})).unwrap()).unwrap();
    std::future::pending::<()>().await;
    drop(browser);
}

/// Only used to clean the exact browser group adopted from our killed fixture
/// worker. waitid proves the leader is still our child and pins its identity.
fn collect_crashed_fixture(pid: u32) -> Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } < 0
    {
        return Err("crash_fixture_owner_not_pinned");
    }
    if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } < 0 {
        return Err("crash_fixture_stop_failed");
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        for child in members(pid)? {
            if child != pid {
                unsafe {
                    libc::waitpid(child as i32, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
        }
        if members(pid)?.iter().all(|child| *child == pid) {
            let status = unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
            if status == pid as i32 {
                break;
            }
        }
        if Instant::now() >= deadline {
            return Err("crash_fixture_cleanup_pending");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !members(pid)?.is_empty() {
        return Err("crash_fixture_cleanup_pending");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measures actual SIGKILL of the test owner; only isolated synthetic Firefox profiles"]
async fn owner_sigkill_observes_native_lifetime_and_safe_recovery() {
    let mut fixture = Fixture::new("owner-sigkill");
    let mut sentinel = fixture.open().await;
    for stage in ["before_endpoint", "after_endpoint", "after_handshake"] {
        for iteration in 0..3 {
            let root = fixture.base.join(format!("crash-{stage}-{iteration}"));
            storage::create_private_dir(&root).unwrap();
            let marker = fixture
                .base
                .join(format!("marker-{stage}-{iteration}.json"));
            let log = storage::create_private_file(
                &fixture.base.join(format!("worker-{stage}-{iteration}.log")),
            )
            .unwrap();
            let mut worker = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "browser::process::bidi_probe::tests::owner_crash_worker",
                    "--nocapture",
                ])
                .env("RDH_D2A_CRASH_ROOT", &root)
                .env("RDH_D2A_CRASH_MARKER", &marker)
                .env("RDH_D2A_CRASH_STAGE", stage)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let identity: Value = loop {
                if let Ok(bytes) = fs::read(&marker)
                    && let Ok(value) = serde_json::from_slice(&bytes)
                {
                    break value;
                }
                assert!(
                    worker.try_wait().unwrap().is_none(),
                    "owner worker failed before marker"
                );
                assert!(
                    Instant::now() < deadline,
                    "owner worker did not reach the requested stage"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let pid = identity["pid"].as_u64().unwrap() as u32;
            worker.kill().await.unwrap();
            // Observe for the existing three-second close budget. This is a
            // feasibility measurement, not a retry of failed browser work.
            let until = Instant::now() + Duration::from_secs(3);
            let mut alive = false;
            while Instant::now() < until {
                alive = crate::process_identity::running(
                    pid,
                    identity["owner"]["start"].as_str().unwrap(),
                )
                .unwrap();
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let remaining = members(pid).unwrap().len();
            let recovered_without_intervention = super::super::recover(&root).is_ok();
            if alive {
                assert!(!recovered_without_intervention);
                assert!(root.join(identity["directory"].as_str().unwrap()).exists());
            }
            fixture.record(json!({"kind":"owner_sigkill","stage":stage,"iteration":iteration,"browser_alive_after_close_budget":alive,"remaining_processes":remaining,"native_recovery_succeeded":recovered_without_intervention,"requires_additional_lifetime_guard":alive}));
            if remaining != 0 {
                collect_crashed_fixture(pid).unwrap();
            }
            super::super::recover(&root).unwrap();
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
            fs::remove_dir(root).unwrap();
            sentinel
                .client
                .as_mut()
                .unwrap()
                .request(Command::Status, Instant::now() + STARTUP)
                .await
                .unwrap();
            fixture.record(json!({"kind":"crash_fixture_cleaned","stage":stage,"iteration":iteration,"remaining_processes":members(pid).unwrap().len(),"other_session_alive":true}));
        }
    }
    let close = sentinel.close(true).await.unwrap();
    fixture.record(json!({"kind":"sentinel_close","close":close}));
    fixture.finish();
}
