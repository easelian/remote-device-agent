//! Real systemd/LaunchDaemon acceptance with a temporary registration. Requires
//! passwordless sudo on a disposable native runner; it is never run by default.
#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]

#[path = "support/initial_binary.rs"]
mod initial_binary;
#[path = "support/native_candidate.rs"]
mod native_candidate;

use futures_util::{SinkExt, StreamExt};
use rdh_agent::{config::sha256_hex, random, storage};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};

const TOKEN: &str = "posix-system-service-test-token-only";

fn run(
    directory: &Path,
    root: bool,
    executable: &Path,
    args: &[&str],
) -> io::Result<(ExitStatus, String)> {
    let log = directory.join(format!("command-{}.log", random::hex_token::<8>().unwrap()));
    let file = fs::File::create(&log)?;
    let mut command = if root {
        let mut command = Command::new("/usr/bin/sudo");
        command.args(["-n", "--"]).arg(executable);
        command
    } else {
        Command::new(executable)
    };
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error,
                    _ => io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{executable:?} {args:?} timed out; log {}", log.display()),
                    ),
                });
            }
        }
    };
    let mut bytes = Vec::new();
    fs::File::open(&log)?.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(io::Error::other(format!(
            "Output exceeds limit: {}",
            log.display()
        )));
    }
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}
fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}
fn absent(pid: u32) -> bool {
    (unsafe { libc::kill(pid as libc::pid_t, 0) }) != 0
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}
struct Fixture {
    directory: PathBuf,
    prefix: PathBuf,
    config: PathBuf,
    state: PathBuf,
    name: String,
    registration: PathBuf,
    may_be_running: bool,
    finished: bool,
}
impl Fixture {
    fn checked(&self, root: bool, executable: &Path, args: &[&str]) -> String {
        let (status, output) = run(&self.directory, root, executable, args).unwrap();
        assert!(
            status.success(),
            "{executable:?} {args:?}: {status}\n{output}"
        );
        output
    }
    fn rejected(&self, root: bool, executable: &Path, args: &[&str], message: &str) {
        let (status, output) = run(&self.directory, root, executable, args).unwrap();
        assert!(
            !status.success() && output.contains(message),
            "Expected {message:?}: {status}\n{output}"
        );
    }
    fn current(&self) -> PathBuf {
        self.prefix.join("current/rdh")
    }
    fn clean(&self) -> bool {
        matches!(fs::symlink_metadata(self.config.with_file_name(format!("{}.node-lock", self.config.file_name().unwrap().to_str().unwrap()))), Err(e) if e.kind() == io::ErrorKind::NotFound)
    }
    fn start(&mut self) {
        self.may_be_running = true;
        #[cfg(target_os = "linux")]
        self.checked(
            true,
            Path::new("/usr/bin/systemctl"),
            &["start", &self.name],
        );
        #[cfg(target_os = "macos")]
        {
            self.checked(
                true,
                Path::new("/bin/launchctl"),
                &["enable", &format!("system/{}", self.name)],
            );
            self.checked(
                true,
                Path::new("/bin/launchctl"),
                &["bootstrap", "system", text(&self.registration)],
            );
        }
    }
    async fn stop(&mut self, pids: &[u32]) {
        #[cfg(target_os = "linux")]
        self.checked(true, Path::new("/usr/bin/systemctl"), &["stop", &self.name]);
        #[cfg(target_os = "macos")]
        self.checked(
            true,
            Path::new("/bin/launchctl"),
            &["bootout", &format!("system/{}", self.name)],
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.clean() || !pids.iter().copied().all(absent) {
            assert!(
                Instant::now() < deadline,
                "System service did not stop cleanly; retained {}",
                self.directory.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.may_be_running = false;
    }
    fn write_root(&self, path: &Path, bytes: &[u8]) {
        let source = self
            .directory
            .join(format!("record-{}.json", random::hex_token::<8>().unwrap()));
        fs::write(&source, bytes).unwrap();
        self.checked(
            true,
            Path::new("/usr/bin/install"),
            &["-m", "644", text(&source), text(path)],
        );
    }
    fn pointer(&self, version: &str) {
        let current = self.prefix.join("current");
        self.checked(true, Path::new("/bin/rm"), &["--", text(&current)]);
        self.checked(
            true,
            Path::new("/bin/ln"),
            &["-s", &format!("versions/{version}"), text(&current)],
        );
    }
    fn pid(&self) -> u32 {
        #[cfg(target_os = "linux")]
        let pid: u32 = self
            .checked(
                true,
                Path::new("/usr/bin/systemctl"),
                &["show", &self.name, "--property=MainPID", "--value"],
            )
            .trim()
            .parse()
            .unwrap();
        #[cfg(target_os = "macos")]
        let pid: u32 = self
            .checked(
                true,
                Path::new("/bin/launchctl"),
                &["print", &format!("system/{}", self.name)],
            )
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid = "))
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid > 0);
        pid
    }
    fn pid_and_executable(&self, version: &str) -> u32 {
        let pid = self.pid();
        #[cfg(target_os = "linux")]
        let actual = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        #[cfg(target_os = "macos")]
        let actual = {
            use std::os::unix::ffi::OsStringExt;
            let mut bytes = vec![0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            let size = unsafe {
                libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32)
            };
            assert!(size > 0 && (size as usize) < bytes.len());
            bytes.truncate(size as usize);
            while bytes.last() == Some(&0) {
                bytes.pop();
            }
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        };
        assert_eq!(
            actual.canonicalize().unwrap(),
            self.prefix
                .join("versions")
                .join(version)
                .join("rdh")
                .canonicalize()
                .unwrap()
        );
        pid
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.finished {
            if let Ok(definition) = fs::read_to_string(&self.registration) {
                eprintln!("Retained fixture definition:\n{definition}");
            }
            #[cfg(target_os = "linux")]
            for (program, arguments) in [
                (
                    "/usr/bin/systemctl",
                    vec!["--no-pager", "status", self.name.as_str()],
                ),
                (
                    "/usr/bin/journalctl",
                    vec!["--no-pager", "-n", "40", "-u", self.name.as_str()],
                ),
            ] {
                if let Ok((_, output)) = run(&self.directory, true, Path::new(program), &arguments)
                {
                    eprintln!("Fixture manager diagnostic:\n{output}");
                }
            }
            #[cfg(target_os = "macos")]
            if let Ok((_, output)) = run(
                &self.directory,
                true,
                Path::new("/bin/launchctl"),
                &["print", &format!("system/{}", self.name)],
            ) {
                eprintln!("Fixture manager diagnostic:\n{output}");
            }
        }
        if self.may_be_running {
            #[cfg(target_os = "linux")]
            let result = run(
                &self.directory,
                true,
                Path::new("/usr/bin/systemctl"),
                &["stop", &self.name],
            );
            #[cfg(target_os = "macos")]
            let result = run(
                &self.directory,
                true,
                Path::new("/bin/launchctl"),
                &["bootout", &format!("system/{}", self.name)],
            );
            if !matches!(result, Ok((status, _)) if status.success()) {
                eprintln!(
                    "System service cleanup uncertain; retained {}",
                    self.directory.display()
                );
                return;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.clean() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !self.clean() {
            eprintln!(
                "System service profile still locked; retained {}",
                self.directory.display()
            );
            return;
        }
        // Only paths generated by this fixture are removed. Keep failed-case
        // records/logs for diagnosis after the temporary service is stopped.
        if self.registration.exists() {
            let _ = run(
                &self.directory,
                true,
                Path::new("/bin/rm"),
                &["--", text(&self.registration)],
            );
        }
        #[cfg(target_os = "linux")]
        let _ = run(
            &self.directory,
            true,
            Path::new("/usr/bin/systemctl"),
            &["daemon-reload"],
        );
        #[cfg(target_os = "macos")]
        let _ = run(
            &self.directory,
            true,
            Path::new("/bin/launchctl"),
            &["enable", &format!("system/{}", self.name)],
        );
        if self.finished {
            let _ = run(
                &self.directory,
                true,
                Path::new("/bin/rm"),
                &["-rf", "--", text(&self.prefix)],
            );
            let _ = fs::remove_dir_all(&self.directory);
        } else {
            eprintln!(
                "Retained system-service fixture: {} and {}",
                self.directory.display(),
                self.prefix.display()
            );
        }
    }
}

struct Peer {
    socket: WebSocketStream<TcpStream>,
    instance: Value,
    next: u64,
}
impl Peer {
    async fn connect(listener: &TcpListener) -> Self {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("System service did not connect")
            .unwrap();
        let socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/agent/posix-system-service");
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer {TOKEN}")
            );
            Ok(response)
        })
        .await
        .unwrap();
        let mut peer = Self {
            socket,
            instance: Value::Null,
            next: 0,
        };
        let hello = peer.receive().await;
        assert_eq!(hello["type"], "hello");
        peer.instance = hello["info"]["capabilities"]["operation_receipts"]["instance_id"].clone();
        assert!(peer.instance.is_string());
        peer.socket
            .send(Message::Text(
                json!({"type":"ready","device_id":"posix-system-service"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        peer
    }
    async fn receive(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match self.socket.next().await.unwrap().unwrap() {
                    Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                    Message::Ping(bytes) => self.socket.send(Message::Pong(bytes)).await.unwrap(),
                    Message::Pong(_) => {}
                    other => panic!("Unexpected service session message: {other:?}"),
                }
            }
        })
        .await
        .expect("Service RPC timeout")
    }
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next.to_string();
        self.socket
            .send(Message::Text(
                json!({"type":"request","id":id,"operation":operation,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        loop {
            let result = self.receive().await;
            if result["type"] == "heartbeat" {
                continue;
            }
            assert_eq!(result["type"], "result");
            assert_eq!(result["id"], id);
            assert!(result.get("error").is_none(), "{operation}: {result}");
            return result["result"].clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "registers a unique system service with passwordless sudo on a disposable native runner"]
async fn system_service_preserves_user_data_across_install_update_rollback_and_interruption() {
    assert_ne!(
        unsafe { libc::getuid() },
        0,
        "Run the test as a non-root runner with sudo"
    );
    let (next, candidate) = native_candidate::load();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let suffix = random::hex_token::<12>().unwrap();
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("rdh system '$%&-{suffix}"));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    #[cfg(target_os = "linux")]
    let name = format!("rdh-system-fixture-{suffix}.service");
    #[cfg(target_os = "macos")]
    let name = format!("com.easelian.rdh-system-fixture-{suffix}");
    #[cfg(target_os = "linux")]
    let registration = PathBuf::from("/etc/systemd/system").join(&name);
    #[cfg(target_os = "macos")]
    let registration = PathBuf::from("/Library/LaunchDaemons").join(format!("{name}.plist"));
    let mut fixture = Fixture {
        config: directory.join("paired '&.json"),
        state: directory.join("state '$%&"),
        directory,
        prefix: PathBuf::from("/var/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rdh root '$%&-{suffix}")),
        name,
        registration,
        may_be_running: false,
        finished: false,
    };
    let user = fixture
        .checked(false, Path::new("/usr/bin/id"), &["-un"])
        .trim()
        .to_owned();
    let work = fixture.directory.join("work");
    for path in [&fixture.state, &work] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let configuration = serde_json::to_vec(&json!({"gateway_url":origin,"device_id":"posix-system-service","token":TOKEN,"roots":{"work":work},"allow_write":true,"allow_shell":true,"shell":"/bin/sh"})).unwrap();
    storage::write_private_new(&fixture.config, &configuration).unwrap();
    let policy = fixture.config.with_extension("policy.json");
    storage::write_private_new(
        &policy,
        br#"{"version":1,"audit":{"enabled":true,"required":true}}"#,
    )
    .unwrap();
    let original_policy = fs::read(&policy).unwrap();
    let original_metadata = fs::metadata(&fixture.config).unwrap();
    let sidecar = fixture.config.with_file_name(format!(
        "{}.startup.json",
        fixture.config.file_name().unwrap().to_str().unwrap()
    ));
    let pending = json!({"request_id":"22222222-2222-4222-8222-222222222222","mode":"service","expires_at":
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 600_000});
    storage::write_private_new(
        &sidecar,
        &serde_json::to_vec(&json!({"version":1,"binding":null,"pending":pending})).unwrap(),
    )
    .unwrap();
    let install = [
        "install-service",
        "--prefix",
        text(&fixture.prefix),
        "--config",
        text(&fixture.config),
        "--state-directory",
        text(&fixture.state),
        "--service-user",
        &user,
        "--name",
        &fixture.name,
    ];
    fixture.rejected(false, initial_binary::path(), &install, "require root");
    assert!(!fixture.prefix.exists());
    fixture.checked(true, initial_binary::path(), &install);
    let bound_sidecar = fs::read(&sidecar).unwrap();
    let startup: Value = serde_json::from_slice(&bound_sidecar).unwrap();
    assert_eq!(startup["pending"], pending);
    assert_eq!(
        startup["binding"],
        json!({"mode":"service","name":fixture.name})
    );
    let current = fixture.current();
    let definition = fs::read(&fixture.registration).unwrap();
    let record_path = fixture.prefix.join(".service-install.json");
    let old: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(old["uid"], unsafe { libc::getuid() });
    assert_eq!(old["config"], text(&fixture.config));
    assert_eq!(old["state"], text(&fixture.state));
    for path in [&record_path, &current, &fixture.registration] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.uid(), 0);
        assert_eq!(metadata.mode() & 0o022, 0);
        assert!(
            fs::OpenOptions::new().write(true).open(path).is_err(),
            "Service user can write {}",
            path.display()
        );
    }
    assert!(!fixture.prefix.join("agent.json").exists());
    let launch_owned = [
        "service-run",
        "--service-name",
        &fixture.name,
        "--config",
        text(&fixture.config),
        "--server",
        &origin,
        "--state-directory",
        text(&fixture.state),
    ]
    .map(str::to_owned);
    let launch: Vec<&str> = launch_owned.iter().map(String::as_str).collect();
    fixture.rejected(true, &current, &launch, "recorded account");
    let binary_file = fixture.directory.join("candidate");
    fs::write(&binary_file, &candidate).unwrap();
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "musl") => "linux-x64-musl",
        ("linux", "aarch64") if cfg!(target_env = "musl") => "linux-arm64-musl",
        ("linux", "x86_64") => "linux-x64-gnu",
        ("linux", "aarch64") => "linux-arm64-gnu",
        ("macos", "x86_64") => "darwin-x64",
        ("macos", "aarch64") => "darwin-arm64",
        _ => panic!("Unsupported platform"),
    };
    let index = fixture.directory.join("channel.json");
    fs::write(&index, serde_json::to_vec(&json!({"schema":2,"channel":"stable","version":next,"platforms":{(platform):{"format":"native-binary","url":"candidate","bytes":candidate.len(),"sha256":sha256_hex(&candidate)}}})).unwrap()).unwrap();
    let upgrade = ["upgrade", "--index", text(&index), "--no-restart"];
    let original = env!("CARGO_PKG_VERSION");
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let pid = fixture.pid_and_executable(original);
    let initial_instance = peer.instance.clone();
    peer.call(
        "write_file",
        json!({"root":"work","path":"kept.txt","content":"system service data"}),
    )
    .await;
    let job = peer.call("start_process", json!({"root":"work","command":"printf '%s\\n' \"$$\"; /bin/sleep 120","wait_seconds":0})).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let child = loop {
        let output = peer
            .call("read_process", json!({"job_id":job["job_id"]}))
            .await;
        if let Some(pid) = output["output"]
            .as_str()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "Child did not report its PID: {output}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    fixture.rejected(true, &current, &upgrade, "active");
    fixture.stop(&[pid, child]).await;
    drop(peer);
    fixture.rejected(false, &current, &upgrade, "require root");
    let mut bad_index: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    bad_index["platforms"][platform]["sha256"] = "0".repeat(64).into();
    let bad_path = fixture.directory.join("bad-index.json");
    fs::write(&bad_path, serde_json::to_vec(&bad_index).unwrap()).unwrap();
    fixture.rejected(
        true,
        &current,
        &["upgrade", "--index", text(&bad_path), "--no-restart"],
        "checksum",
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&record_path).unwrap()).unwrap(),
        old
    );
    assert!(!fixture.prefix.join("versions").join(&next).exists());
    let held =
        rdh_agent::profile_lock::ProfileLocks::acquire(std::slice::from_ref(&fixture.config))
            .unwrap();
    fixture.rejected(true, &current, &upgrade, "locked");
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&record_path).unwrap()).unwrap(),
        old
    );
    assert!(!fixture.prefix.join(".service-update-pending.json").exists());
    drop(held);
    fixture.checked(true, &current, &upgrade);
    let updated: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(updated["current"]["version"], next);
    assert_eq!(updated["previous"], old["current"]);
    assert_eq!(fs::read(&fixture.registration).unwrap(), definition);
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let pid = fixture.pid_and_executable(&next);
    assert_ne!(peer.instance, initial_instance);
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"kept.txt"}))
            .await["content"],
        "system service data"
    );
    fixture.stop(&[pid]).await;
    drop(peer);
    fixture.checked(true, &current, &["rollback", "--no-restart"]);
    let rolled: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(rolled["current"], old["current"]);
    let mut switched = rolled.clone();
    switched["current"] = updated["current"].clone();
    switched["previous"] = rolled["current"].clone();
    for boundary in 0..3 {
        let pending = json!({"schema":1,"old":rolled,"new":switched});
        fixture.write_root(
            &fixture.prefix.join(".service-update-pending.json"),
            &serde_json::to_vec(&pending).unwrap(),
        );
        if boundary >= 1 {
            fixture.pointer(&next);
        }
        if boundary == 2 {
            fixture.write_root(&record_path, &serde_json::to_vec(&switched).unwrap());
        }
        fixture.rejected(false, &current, &launch, "interrupted");
        fixture.checked(true, &current, &["upgrade", "--recover"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&record_path).unwrap()).unwrap(),
            rolled
        );
        assert_eq!(
            fs::read_link(fixture.prefix.join("current")).unwrap(),
            PathBuf::from(format!("versions/{original}"))
        );
        assert!(!fixture.prefix.join(".service-update-pending.json").exists());
    }
    // A locally altered registration cannot be silently overwritten by update.
    let mut altered = definition.clone();
    altered.extend_from_slice(b"\n");
    fixture.write_root(&fixture.registration, &altered);
    fixture.rejected(true, &current, &upgrade, "definition changed");
    fixture.write_root(&fixture.registration, &definition);
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let pid = fixture.pid_and_executable(original);
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"kept.txt"}))
            .await["content"],
        "system service data"
    );
    fixture.stop(&[pid]).await;
    assert_eq!(fs::read(&fixture.config).unwrap(), configuration);
    assert_eq!(
        fs::metadata(&fixture.config).unwrap().ino(),
        original_metadata.ino()
    );
    assert_eq!(
        fs::metadata(&fixture.config).unwrap().uid(),
        original_metadata.uid()
    );
    assert_eq!(fs::read(&policy).unwrap(), original_policy);
    assert_eq!(fs::read(&sidecar).unwrap(), bound_sidecar);
    assert_eq!(fs::read(&fixture.registration).unwrap(), definition);
    let audit = fixture.state.join("remote-device-hub/audit");
    assert!(audit.is_dir());
    assert!(
        fs::read_dir(&audit)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.metadata().unwrap().len() > 0)
    );
    fixture.finished = true;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the actual legacy build, native artifact and a disposable sudo runner"]
async fn legacy_system_service_handoff_preserves_identity_and_can_be_undone() {
    assert_ne!(unsafe { libc::getuid() }, 0);
    let source =
        PathBuf::from(std::env::var_os("RDH_LEGACY_SOURCE").expect("legacy source required"));
    let node =
        PathBuf::from(std::env::var_os("RDH_LEGACY_NODE").expect("legacy Node binary required"));
    assert!(source.is_absolute() && node.is_absolute());
    let package: Value =
        serde_json::from_slice(&fs::read(source.join("package.json")).unwrap()).unwrap();
    assert_eq!(package["name"], "remote-device-hub");
    let legacy_version = package["version"].as_str().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let suffix = random::hex_token::<12>().unwrap();
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("rdh legacy '$%&-{suffix}"));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    #[cfg(target_os = "linux")]
    let name = format!("rdh-legacy-system-{suffix}.service");
    #[cfg(target_os = "macos")]
    let name = format!("com.easelian.rdh-legacy-system-{suffix}");
    #[cfg(target_os = "linux")]
    let registration = PathBuf::from("/etc/systemd/system").join(&name);
    #[cfg(target_os = "macos")]
    let registration = PathBuf::from("/Library/LaunchDaemons").join(format!("{name}.plist"));
    let root = PathBuf::from("/var/tmp").canonicalize().unwrap();
    let mut fixture = Fixture {
        config: directory.join("agent.json"),
        state: directory.join("state '$%&"),
        directory,
        prefix: root.join(format!("rdh-native-migration-{suffix}")),
        name,
        registration,
        may_be_running: false,
        finished: false,
    };
    let legacy = root.join(format!("rdh-legacy-migration-{suffix}"));
    let legacy_dir = legacy.join("versions").join(legacy_version);
    let user = fixture
        .checked(false, Path::new("/usr/bin/id"), &["-un"])
        .trim()
        .to_owned();
    #[cfg(target_os = "linux")]
    let gid = unsafe { libc::getgid() };
    let work = fixture.directory.join("work");
    for path in [&fixture.state, &work] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config = serde_json::to_vec(
        &json!({"gateway_url":origin,"device_id":"posix-system-service","token":TOKEN,
        "roots":{"work":work},"allow_write":true,"allow_shell":true,"shell":"/bin/sh"}),
    )
    .unwrap();
    storage::write_private_new(&fixture.config, &config).unwrap();
    let config_meta = fs::metadata(&fixture.config).unwrap();
    let policy = fixture.config.with_extension("policy.json");
    let policy_bytes = br#"{"version":1,"audit":{"enabled":true,"required":true}}"#;
    storage::write_private_new(&policy, policy_bytes).unwrap();
    let sidecar = fixture.config.with_file_name("agent.json.startup.json");
    let pending = json!({"request_id":"33333333-3333-4333-8333-333333333333","mode":"service","expires_at":
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 600_000});
    let startup = serde_json::to_vec(
        &json!({"version":1,"binding":{"mode":"service","name":fixture.name},"pending":pending}),
    )
    .unwrap();
    storage::write_private_new(&sidecar, &startup).unwrap();
    let shell = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let wrapper = format!(
        "#!/bin/sh\nexport XDG_STATE_HOME={}\ncd {} || exit 1\nexec {} '--no-browser' '--config' {} '--server' {} '--root' {} --recover-lock\n",
        shell(text(&fixture.state)),
        shell(text(&fixture.state)),
        shell(text(&legacy.join("current/rdh"))),
        shell(text(&fixture.config)),
        shell(&origin),
        shell(&format!("work={}", text(&work)))
    );
    let local_wrapper = fixture.directory.join("legacy-wrapper");
    fs::write(&local_wrapper, &wrapper).unwrap();
    let launcher = fixture.directory.join("legacy-launcher");
    fs::write(&launcher, include_bytes!("fixtures/legacy-launcher.sh")).unwrap();
    for path in [legacy_dir.join("bin"), legacy_dir.join("bundle")] {
        fixture.checked(true, Path::new("/bin/mkdir"), &["-p", text(&path)]);
    }
    for (from, to, executable) in [
        (node, legacy_dir.join("node"), true),
        (launcher, legacy_dir.join("rdh"), true),
        (
            source.join("package.json"),
            legacy_dir.join("package.json"),
            false,
        ),
        (
            source.join("bin/remote-device-hub.mjs"),
            legacy_dir.join("bin/remote-device-hub.mjs"),
            false,
        ),
        (
            source.join("bundle/agent.mjs"),
            legacy_dir.join("bundle/agent.mjs"),
            false,
        ),
        (local_wrapper, legacy.join("run-agent"), true),
    ] {
        fixture.checked(
            true,
            Path::new("/usr/bin/install"),
            &[
                "-m",
                if executable { "755" } else { "644" },
                text(&from),
                text(&to),
            ],
        );
    }
    fixture.checked(
        true,
        Path::new("/bin/chmod"),
        &["-R", "go-w", text(&legacy)],
    );
    fixture.checked(
        true,
        Path::new("/bin/ln"),
        &[
            "-s",
            &format!("versions/{legacy_version}"),
            text(&legacy.join("current")),
        ],
    );
    assert_eq!(
        fixture
            .checked(false, &legacy.join("current/rdh"), &["--version"])
            .trim(),
        legacy_version
    );
    #[cfg(target_os = "linux")]
    let original_definition = format!(
        "[Unit]\nDescription=Remote Device Hub Agent\nAfter=network-online.target\n\n[Service]\nUser={user}\nGroup={gid}\nExecStart=\"{}\"\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n",
        text(&legacy.join("run-agent"))
    );
    #[cfg(target_os = "macos")]
    let original_definition = {
        let xml = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>{}</string></array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>UserName</key><string>{user}</string>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict></plist>\n",
            fixture.name,
            xml(text(&legacy.join("run-agent"))),
            xml(text(&fixture.state))
        )
    };
    let input = fixture.directory.join("original-definition");
    fs::write(&input, &original_definition).unwrap();
    fixture.checked(
        true,
        Path::new("/usr/bin/install"),
        &["-m", "644", text(&input), text(&fixture.registration)],
    );
    #[cfg(target_os = "linux")]
    fixture.checked(true, Path::new("/usr/bin/systemctl"), &["daemon-reload"]);
    fixture.start();
    let mut old_peer = Peer::connect(&listener).await;
    let old_pid = fixture.pid();
    old_peer
        .call(
            "write_file",
            json!({"root":"work","path":"handoff.txt","content":"from legacy"}),
        )
        .await;
    let install_owned = [
        "install-service",
        "--prefix",
        text(&fixture.prefix),
        "--config",
        text(&fixture.config),
        "--state-directory",
        text(&fixture.state),
        "--service-user",
        &user,
        "--name",
        &fixture.name,
        "--migrate-from",
        text(&legacy),
    ]
    .map(str::to_owned);
    let install: Vec<&str> = install_owned.iter().map(String::as_str).collect();
    // A live legacy Agent owns the same profile lock. No registration changes.
    fixture.rejected(true, initial_binary::path(), &install, "profile");
    assert_eq!(
        fs::read(&fixture.registration).unwrap(),
        original_definition.as_bytes()
    );
    fixture.checked(
        true,
        Path::new("/bin/rm"),
        &["-rf", "--", text(&fixture.prefix)],
    );
    fixture.stop(&[old_pid]).await;
    drop(old_peer);
    // A changed definition must not be silently adopted or overwritten.
    let changed = format!("{original_definition}\n# external change\n");
    fs::write(&input, &changed).unwrap();
    fixture.checked(
        true,
        Path::new("/usr/bin/install"),
        &["-m", "644", text(&input), text(&fixture.registration)],
    );
    fixture.rejected(
        true,
        initial_binary::path(),
        &install,
        "differs from the recognized",
    );
    assert_eq!(fs::read(&fixture.registration).unwrap(), changed.as_bytes());
    fixture.checked(
        true,
        Path::new("/bin/rm"),
        &["-rf", "--", text(&fixture.prefix)],
    );
    fs::write(&input, &original_definition).unwrap();
    fixture.checked(
        true,
        Path::new("/usr/bin/install"),
        &["-m", "644", text(&input), text(&fixture.registration)],
    );
    fixture.checked(true, initial_binary::path(), &install);
    let current = fixture.current();
    let native_definition = fs::read(&fixture.registration).unwrap();
    let binary_digest = sha256_hex(&fs::read(legacy_dir.join("node")).unwrap());
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let pid = fixture.pid_and_executable(env!("CARGO_PKG_VERSION"));
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"handoff.txt"}))
            .await["content"],
        "from legacy"
    );
    peer.call(
        "write_file",
        json!({"root":"work","path":"handoff.txt","content":"from Rust"}),
    )
    .await;
    let undo_owned = [
        "install-service",
        "--undo-migration",
        "--prefix",
        text(&fixture.prefix),
    ]
    .map(str::to_owned);
    let undo: Vec<&str> = undo_owned.iter().map(String::as_str).collect();
    fixture.rejected(true, &current, &undo, "active");
    fixture.stop(&[pid]).await;
    drop(peer);
    fixture.checked(true, &current, &undo);
    assert_eq!(
        fs::read(&fixture.registration).unwrap(),
        original_definition.as_bytes()
    );
    assert!(fixture.prefix.join(".service-install-pending").exists());
    fixture.rejected(
        false,
        &current,
        &[
            "service-run",
            "--service-name",
            &fixture.name,
            "--config",
            text(&fixture.config),
            "--server",
            &origin,
            "--state-directory",
            text(&fixture.state),
        ],
        "incomplete",
    );
    // Restore the fixture's pre-undo snapshot before simulating interrupted
    // transitions. This reset is not part of the migration command.
    fixture.checked(
        true,
        Path::new("/bin/rm"),
        &[
            "--",
            text(&fixture.prefix.join(".service-migration-undone")),
            text(&fixture.prefix.join(".service-install-pending")),
        ],
    );
    // Reconstruct the gap between backup and initial intent, then both durable
    // boundaries of apply and undo, including completed registration publication.
    let intent_input = fixture.directory.join("intent");
    let barrier_input = fixture.directory.join("barrier");
    fs::write(&barrier_input, b"interrupted service migration\n").unwrap();
    for (intent, observed, expected) in [
        (
            None,
            original_definition.as_bytes(),
            native_definition.as_slice(),
        ),
        (
            Some("apply\n"),
            original_definition.as_bytes(),
            native_definition.as_slice(),
        ),
        (
            Some("apply\n"),
            native_definition.as_slice(),
            native_definition.as_slice(),
        ),
        (
            Some("undo\n"),
            native_definition.as_slice(),
            original_definition.as_bytes(),
        ),
        (
            Some("undo\n"),
            original_definition.as_bytes(),
            original_definition.as_bytes(),
        ),
    ] {
        if let Some(intent) = intent {
            fs::write(&intent_input, intent).unwrap();
            fixture.checked(
                true,
                Path::new("/usr/bin/install"),
                &[
                    "-m",
                    "644",
                    text(&intent_input),
                    text(&fixture.prefix.join(".service-migration-intent")),
                ],
            );
        }
        fs::write(&input, observed).unwrap();
        for (from, to) in [
            (
                &barrier_input,
                fixture.prefix.join(".service-install-pending"),
            ),
            (&input, fixture.registration.clone()),
        ] {
            fixture.checked(
                true,
                Path::new("/usr/bin/install"),
                &["-m", "644", text(from), text(&to)],
            );
        }
        fixture.checked(
            true,
            &current,
            &[
                "install-service",
                "--recover-migration",
                "--prefix",
                text(&fixture.prefix),
            ],
        );
        assert_eq!(fs::read(&fixture.registration).unwrap(), expected);
        assert_eq!(
            fixture.prefix.join(".service-install-pending").exists(),
            intent == Some("undo\n")
        );
        assert!(!fixture.prefix.join(".service-migration-intent").exists());
    }
    // Actual undo has restored the original executable selection and profile.
    fixture.start();
    let mut returned = Peer::connect(&listener).await;
    let returned_pid = fixture.pid();
    assert_eq!(
        returned
            .call("read_file", json!({"root":"work","path":"handoff.txt"}))
            .await["content"],
        "from Rust"
    );
    fixture.stop(&[returned_pid]).await;
    assert_eq!(
        sha256_hex(&fs::read(legacy_dir.join("node")).unwrap()),
        binary_digest
    );
    assert_eq!(fs::read(&fixture.config).unwrap(), config);
    assert_eq!(
        fs::metadata(&fixture.config).unwrap().ino(),
        config_meta.ino()
    );
    assert_eq!(fs::read(&policy).unwrap(), policy_bytes);
    assert_eq!(fs::read(&sidecar).unwrap(), startup);
    assert_eq!(
        fs::read(&fixture.registration).unwrap(),
        original_definition.as_bytes()
    );
    let audit = fixture.state.join("remote-device-hub/audit");
    assert!(
        audit.is_dir()
            && fs::read_dir(audit)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.metadata().unwrap().len() > 0)
    );
    fixture.checked(true, Path::new("/bin/rm"), &["-rf", "--", text(&legacy)]);
    fixture.finished = true;
}
