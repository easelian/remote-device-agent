//! A temporary LaunchAgent runs the installed binary with an empty PATH.
#![cfg(all(target_os = "macos", feature = "experimental-macos-install"))]

#[path = "support/initial_binary.rs"]
mod initial_binary;

use futures_util::{SinkExt, StreamExt};
use rdh_agent::{config::sha256_hex, random, storage};
use serde_json::{Value, json};
use std::{
    fs, io,
    io::Read,
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt, symlink},
    },
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

#[path = "support/native_candidate.rs"]
mod native_candidate;

const TOKEN: &str = "native-launchd-fixture-token-only";

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

// File output avoids an inherited pipe keeping the test waiting after a
// command exits. Every invoked command has a deadline and is reaped here.
fn run(directory: &Path, executable: &Path, args: &[&str]) -> io::Result<(ExitStatus, String)> {
    let log = directory.join(format!("command-{}.log", random::hex_token::<8>().unwrap()));
    let output = fs::File::create(&log)?;
    let mut child = Command::new(executable)
        .args(args)
        .env_clear()
        .env("PATH", "")
        .env("HOME", directory.join("home"))
        .env("XDG_CONFIG_HOME", directory.join("config"))
        .env("XDG_STATE_HOME", directory.join("state"))
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error,
                    _ => io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("Command {args:?} timed out; log: {}", log.display()),
                    ),
                });
            }
        }
    };
    let mut bytes = Vec::new();
    fs::File::open(&log)?.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(io::Error::other(format!(
            "Command output exceeds limit: {}",
            log.display()
        )));
    }
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn absent(pid: u32) -> bool {
    (unsafe { libc::kill(pid as libc::pid_t, 0) }) != 0
        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn process_path(pid: u32) -> PathBuf {
    let mut bytes = vec![0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length =
        unsafe { libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    assert!(length > 0 && (length as usize) < bytes.len());
    bytes.truncate(length as usize);
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
        .canonicalize()
        .unwrap()
}

struct Fixture {
    directory: PathBuf,
    prefix: PathBuf,
    config: PathBuf,
    plist: PathBuf,
    domain: String,
    target: String,
    booted: bool,
}
impl Fixture {
    fn checked(&self, executable: &Path, args: &[&str]) -> String {
        let (status, output) = run(&self.directory, executable, args).unwrap();
        assert!(
            status.success(),
            "{executable:?} {args:?}: {status}\n{output}"
        );
        output
    }
    fn launchctl(&self, args: &[&str]) -> String {
        self.checked(Path::new("/bin/launchctl"), args)
    }
    fn boot_setting(&self) -> Option<String> {
        let label = self.target.rsplit('/').next().unwrap();
        self.launchctl(&["print-disabled", &self.domain])
            .lines()
            .find(|line| line.contains(&format!("\"{label}\"")))
            .map(|line| line.trim().to_owned())
    }
    fn clean(&self) -> bool {
        let marker_clear = match fs::read_dir(self.prefix.join(".running")) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        let lock = self.config.with_file_name(format!(
            "{}.node-lock",
            self.config.file_name().unwrap().to_str().unwrap()
        ));
        marker_clear
            && matches!(fs::symlink_metadata(lock), Err(error) if error.kind() == io::ErrorKind::NotFound)
    }
    fn start(&mut self) {
        self.booted = true; // An uncertain bootstrap still requires cleanup.
        self.launchctl(&["bootstrap", &self.domain, self.plist.to_str().unwrap()]);
    }
    fn running_binary(&self, version: &str) -> u32 {
        let entries: Vec<_> = fs::read_dir(self.prefix.join(".running"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        let record: Value =
            serde_json::from_slice(&storage::read_private(&entries[0]).unwrap()).unwrap();
        assert_eq!(record["config"], self.config.to_str().unwrap());
        let pid = u32::try_from(record["pid"].as_u64().unwrap()).unwrap();
        assert!(pid > 0 && pid <= i32::MAX as u32);
        let mut bytes = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let length = unsafe {
            libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32)
        };
        assert!(
            length > 0 && (length as usize) < bytes.len(),
            "Cannot identify launched executable"
        );
        bytes.truncate(length as usize);
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        let actual = PathBuf::from(std::ffi::OsString::from_vec(bytes))
            .canonicalize()
            .unwrap();
        assert_eq!(
            actual,
            self.prefix
                .join("versions")
                .join(version)
                .join("rdh")
                .canonicalize()
                .unwrap()
        );
        pid
    }
    async fn stop(&mut self, processes: &[u32]) {
        self.launchctl(&["bootout", &self.target]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.clean() || !processes.iter().copied().all(absent) {
            assert!(
                Instant::now() < deadline,
                "LaunchAgent cleanup was not confirmed; retained {}",
                self.directory.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (status, _) = run(
            &self.directory,
            Path::new("/bin/launchctl"),
            &["print", &self.target],
        )
        .unwrap();
        assert!(
            !status.success(),
            "Temporary LaunchAgent is still registered"
        );
        self.booted = false;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Ok((_, output)) = run(
                &self.directory,
                Path::new("/bin/launchctl"),
                &["print", &self.target],
            )
        {
            eprintln!("Failed LaunchAgent diagnostic:\n{output}");
        }
        if self.booted {
            let result = run(
                &self.directory,
                Path::new("/bin/launchctl"),
                &["bootout", &self.target],
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            while !self.clean() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !matches!(result, Ok((status, _)) if status.success()) || !self.clean() {
                eprintln!(
                    "Retaining launchd fixture with unconfirmed cleanup: {}",
                    self.directory.display()
                );
                return;
            }
        }
        if std::thread::panicking() {
            eprintln!("Retained LaunchAgent fixture: {}", self.directory.display());
        } else {
            let _ = fs::remove_dir_all(&self.directory);
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
        let (stream, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
            .await
            .expect("LaunchAgent did not connect to the fixture Hub")
            .unwrap();
        let socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/agent/native-launchd");
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
        peer.send(json!({"type":"ready","device_id":"native-launchd"}))
            .await;
        peer
    }
    async fn send(&mut self, value: Value) {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
    async fn receive(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match self.socket.next().await.unwrap().unwrap() {
                    Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                    Message::Ping(data) => self.socket.send(Message::Pong(data)).await.unwrap(),
                    Message::Pong(_) => {}
                    other => panic!("Unexpected launchd session message: {other:?}"),
                }
            }
        })
        .await
        .expect("LaunchAgent RPC timed out")
    }
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next.to_string();
        self.send(json!({"type":"request","id":id,"operation":operation,"params":params}))
            .await;
        loop {
            let value = self.receive().await;
            if value["type"] == "heartbeat" {
                continue;
            }
            assert_eq!(value["type"], "result");
            assert_eq!(value["id"], id);
            assert!(value.get("error").is_none(), "{operation}: {value}");
            return value["result"].clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "bootstraps and removes a uniquely named LaunchAgent on a macOS runner with a GUI domain"]
async fn native_launchagent_runs_installed_versions_and_preserves_identity_across_offline_updates()
{
    let (next, bytes) = native_candidate::load();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let suffix = random::hex_token::<12>().unwrap();
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("rdh launchd ' &-{suffix}"));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let label = format!("com.easelian.rdh-test-{suffix}");
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let mut fixture = Fixture {
        prefix: directory.join("native install"),
        config: directory.join("device '&.json"),
        plist: directory.join(format!("{label}.plist")),
        target: format!("{domain}/{label}"),
        domain,
        directory,
        booted: false,
    };
    fixture.launchctl(&["print-disabled", &fixture.domain]);
    for name in ["home", "work"] {
        fs::create_dir(fixture.directory.join(name)).unwrap();
    }
    let work = fixture.directory.join("work");
    let config = serde_json::to_vec(&json!({"gateway_url":origin,"device_id":"native-launchd",
        "token":TOKEN,"roots":{"work":work},"allow_write":true,"allow_shell":true,"shell":"/bin/sh"})).unwrap();
    storage::write_private_new(&fixture.config, &config).unwrap();
    storage::write_private_new(
        &fixture.config.with_extension("policy.json"),
        br#"{"version":1,"audit":{"enabled":false}}"#,
    )
    .unwrap();
    fixture.checked(
        initial_binary::path(),
        &[
            "install-native",
            "--prefix",
            fixture.prefix.to_str().unwrap(),
            "--config",
            fixture.config.to_str().unwrap(),
        ],
    );
    let current = fixture.prefix.join("current/rdh");
    let mut plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array>"
    );
    for argument in [
        current.to_str().unwrap(),
        "--config",
        fixture.config.to_str().unwrap(),
        "--server",
        &origin,
        "--no-browser",
    ] {
        plist.push_str(&format!("<string>{}</string>", xml(argument)));
    }
    plist.push_str(&format!("</array><key>WorkingDirectory</key><string>{}</string><key>EnvironmentVariables</key><dict><key>PATH</key><string></string><key>HOME</key><string>{}</string></dict><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string><key>RunAtLoad</key><true/></dict></plist>",
        xml(fixture.directory.to_str().unwrap()), xml(fixture.directory.join("home").to_str().unwrap()),
        xml(fixture.directory.join("agent.log").to_str().unwrap()), xml(fixture.directory.join("agent.log").to_str().unwrap())));
    storage::write_private_new(&fixture.plist, plist.as_bytes()).unwrap();
    let candidate = fixture.directory.join("candidate");
    fs::write(&candidate, &bytes).unwrap();
    let platform = if cfg!(target_arch = "aarch64") {
        "darwin-arm64"
    } else {
        "darwin-x64"
    };
    let index = fixture.directory.join("channel.json");
    fs::write(&index, serde_json::to_vec(&json!({"schema":2,"channel":"stable","version":next,
        "platforms":{(platform):{"format":"native-binary","url":"candidate","sha256":sha256_hex(&bytes),"bytes":bytes.len()}}})).unwrap()).unwrap();
    let upgrade = [
        "upgrade",
        "--index",
        index.to_str().unwrap(),
        "--no-restart",
    ];
    let original = env!("CARGO_PKG_VERSION");
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let owner = fixture.running_binary(original);
    let initial_instance = peer.instance.clone();
    peer.call(
        "write_file",
        json!({"root":"work","path":"kept.txt","content":"launchd data"}),
    )
    .await;
    let job = peer.call("start_process", json!({"root":"work","command":"printf '%s\\n' \"$$\"; /bin/sleep 120","wait_seconds":0})).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let child = loop {
        let value = peer
            .call("read_process", json!({"job_id":job["job_id"]}))
            .await;
        if let Some(pid) = value["output"]
            .as_str()
            .and_then(|output| output.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "LaunchAgent child did not report its identity: {value}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let (status, output) = run(&fixture.directory, &current, &upgrade).unwrap();
    assert!(
        !status.success() && output.contains("active"),
        "Live installation must refuse an offline switch: {output}"
    );
    fixture.running_binary(original);
    fixture.stop(&[owner, child]).await;
    drop(peer);
    fixture.checked(&current, &upgrade);
    assert_eq!(fs::read(&fixture.plist).unwrap(), plist.as_bytes());
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let owner = fixture.running_binary(&next);
    assert_ne!(peer.instance, initial_instance);
    let updated_instance = peer.instance.clone();
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"kept.txt"}))
            .await["content"],
        "launchd data"
    );
    fixture.stop(&[owner]).await;
    drop(peer);
    fixture.checked(&current, &["rollback", "--no-restart"]);
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let owner = fixture.running_binary(original);
    assert_ne!(peer.instance, initial_instance);
    assert_ne!(peer.instance, updated_instance);
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"kept.txt"}))
            .await["content"],
        "launchd data"
    );
    fixture.stop(&[owner]).await;
    assert_eq!(fs::read(&fixture.config).unwrap(), config);
    assert_eq!(fs::read(&fixture.plist).unwrap(), plist.as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a native GUI runner and the actual legacy Node build"]
async fn legacy_launchagent_wrappers_migrate_and_restore_without_changing_the_profile() {
    let source =
        PathBuf::from(std::env::var_os("RDH_LEGACY_SOURCE").expect("legacy source required"));
    let node =
        PathBuf::from(std::env::var_os("RDH_LEGACY_NODE").expect("legacy Node binary required"));
    let package: Value =
        serde_json::from_slice(&fs::read(source.join("package.json")).unwrap()).unwrap();
    let version = package["version"].as_str().unwrap();
    for installer in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let suffix = random::hex_token::<12>().unwrap();
        let directory = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("rdh legacy user '$&-{suffix}"));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let label = format!("com.easelian.rdh-legacy-user-{suffix}");
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let home = directory.join("home");
        let agents = home.join("Library/LaunchAgents");
        fs::create_dir_all(&agents).unwrap();
        let mut fixture = Fixture {
            prefix: directory.join("native install"),
            config: directory.join("device '&.json"),
            plist: agents.join(format!("{label}.plist")),
            target: format!("{domain}/{label}"),
            domain,
            directory,
            booted: false,
        };
        let legacy = fixture.directory.join("legacy install");
        let old_version = legacy.join("versions").join(version);
        fs::create_dir_all(old_version.join("bin")).unwrap();
        fs::create_dir_all(old_version.join("bundle")).unwrap();
        for (from, to, executable) in [
            (node.clone(), old_version.join("node"), true),
            (
                source.join("package.json"),
                old_version.join("package.json"),
                false,
            ),
            (
                source.join("bin/remote-device-hub.mjs"),
                old_version.join("bin/remote-device-hub.mjs"),
                false,
            ),
            (
                source.join("bundle/agent.mjs"),
                old_version.join("bundle/agent.mjs"),
                false,
            ),
        ] {
            fs::copy(from, &to).unwrap();
            fs::set_permissions(
                to,
                fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
            )
            .unwrap();
        }
        fs::write(
            old_version.join("rdh"),
            include_bytes!("fixtures/legacy-launcher.sh"),
        )
        .unwrap();
        fs::set_permissions(old_version.join("rdh"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(format!("versions/{version}"), legacy.join("current")).unwrap();
        assert_eq!(
            fixture
                .checked(&legacy.join("current/rdh"), &["--version"])
                .trim(),
            version
        );
        let work = fixture.directory.join("work");
        fs::create_dir(&work).unwrap();
        fs::create_dir(fixture.directory.join("state")).unwrap();
        let config = serde_json::to_vec(
            &json!({"gateway_url":origin,"device_id":"native-launchd", "token":TOKEN,
            "roots":{"work":work},"allow_write":true,"allow_shell":true,"shell":"/bin/sh"}),
        )
        .unwrap();
        storage::write_private_new(&fixture.config, &config).unwrap();
        let config_inode = fs::metadata(&fixture.config).unwrap().ino();
        let policy = fixture.config.with_extension("policy.json");
        let policy_bytes = br#"{"version":1,"audit":{"enabled":false}}"#;
        storage::write_private_new(&policy, policy_bytes).unwrap();
        let sidecar = PathBuf::from(format!("{}.startup.json", text(&fixture.config)));
        let pending = json!({"request_id":"55555555-5555-4555-8555-555555555555","mode":"none","expires_at":
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 600_000});
        let startup = serde_json::to_vec(
            &json!({"version":1,"binding":{"mode":"user","name":label},"pending":pending}),
        )
        .unwrap();
        storage::write_private_new(&sidecar, &startup).unwrap();
        let shell = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let wrapper = if installer {
            legacy.join("run-agent")
        } else {
            PathBuf::from(format!("{}.startup-run", text(&fixture.config)))
        };
        let old = if installer {
            format!(
                "#!/bin/sh\nexport XDG_CONFIG_HOME={}\nexport XDG_STATE_HOME={}\ncd {} || exit 1\nexec {} '--no-browser' '--config' {} '--server' {} --recover-lock\n",
                shell(text(&fixture.directory.join("config"))),
                shell(text(&fixture.directory.join("state"))),
                shell(text(&home)),
                shell(text(&legacy.join("current/rdh"))),
                shell(text(&fixture.config)),
                shell(&origin)
            )
        } else {
            format!(
                "#!/bin/sh\ncd {} || exit 1\nexec {} {} '--config' {} '--server' {} '--no-browser' '--recover-lock'\n",
                shell(text(fixture.config.parent().unwrap())),
                shell(text(&old_version.join("node"))),
                shell(text(&legacy.join("current/bin/remote-device-hub.mjs"))),
                shell(text(&fixture.config)),
                shell(&origin)
            )
        };
        fs::write(&wrapper, &old).unwrap();
        fs::set_permissions(
            &wrapper,
            fs::Permissions::from_mode(if installer { 0o755 } else { 0o700 }),
        )
        .unwrap();
        let plist = if installer {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>{}</string></array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict></plist>\n",
                xml(text(&wrapper)).replace('"', "&quot;"),
                xml(text(&home)).replace('"', "&quot;")
            )
        } else {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string></array><key>RunAtLoad</key><true/></dict></plist>\n",
                xml(text(&wrapper))
            )
        };
        storage::write_private_new(&fixture.plist, plist.as_bytes()).unwrap();
        fixture.launchctl(&["enable", &fixture.target]);
        fixture.start();
        let mut old_peer = Peer::connect(&listener).await;
        let old_pid: u32 = fixture
            .launchctl(&["print", &fixture.target])
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid = "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(process_path(old_pid), old_version.join("node"));
        old_peer
            .call(
                "write_file",
                json!({"root":"work","path":"handoff.txt","content":"old identity"}),
            )
            .await;
        fixture.stop(&[old_pid]).await;
        drop(old_peer);
        fixture.checked(
            initial_binary::path(),
            &[
                "install-native",
                "--prefix",
                text(&fixture.prefix),
                "--config",
                text(&fixture.config),
            ],
        );
        let current = fixture.prefix.join("current/rdh");
        let apply_owned = [
            "migrate",
            "--offline",
            "--prefix",
            text(&legacy),
            "--config",
            text(&fixture.config),
            "--service-unit",
            &label,
        ]
        .map(str::to_owned);
        let apply: Vec<&str> = apply_owned.iter().map(String::as_str).collect();
        let mut undo_owned = apply_owned.clone();
        undo_owned[1] = "--undo".into();
        let undo: Vec<&str> = undo_owned.iter().map(String::as_str).collect();
        let enabled = fixture.boot_setting();
        fixture.checked(&current, &apply);
        let native_wrapper = fs::read(&wrapper).unwrap();
        assert_ne!(native_wrapper, old.as_bytes());
        fixture.checked(&current, &apply); // Repeat after a completed atomic publication.
        assert_eq!(fs::read(&wrapper).unwrap(), native_wrapper);
        assert_eq!(fixture.boot_setting(), enabled);
        assert_eq!(fs::read(&fixture.plist).unwrap(), plist.as_bytes());
        fixture.start();
        let mut native_peer = Peer::connect(&listener).await;
        let native_pid = fixture.running_binary(env!("CARGO_PKG_VERSION"));
        assert_eq!(
            native_peer
                .call("read_file", json!({"root":"work","path":"handoff.txt"}))
                .await["content"],
            "old identity"
        );
        native_peer
            .call(
                "write_file",
                json!({"root":"work","path":"handoff.txt","content":"native identity"}),
            )
            .await;
        let (status, output) = run(&fixture.directory, &current, &undo).unwrap();
        assert!(!status.success() && output.contains("active"), "{output}");
        fixture.stop(&[native_pid]).await;
        drop(native_peer);
        // Unrelated plist edits and wrapper edits must remain untouched.
        storage::write_private_replace(
            &fixture.plist,
            format!("{plist}\n<!-- external -->\n").as_bytes(),
        )
        .unwrap();
        assert!(
            !run(&fixture.directory, &current, &undo)
                .unwrap()
                .0
                .success()
        );
        storage::write_private_replace(&fixture.plist, plist.as_bytes()).unwrap();
        let changed = b"#!/bin/sh\nexit 3\n";
        fs::write(&wrapper, changed).unwrap();
        assert!(
            !run(&fixture.directory, &current, &undo)
                .unwrap()
                .0
                .success()
        );
        assert_eq!(fs::read(&wrapper).unwrap(), changed);
        fs::write(&wrapper, &native_wrapper).unwrap();
        // Reconstruct the pre-publication boundary: durable record plus old wrapper.
        fs::write(&wrapper, &old).unwrap();
        fixture.checked(&current, &apply);
        assert_eq!(fs::read(&wrapper).unwrap(), native_wrapper);
        fixture.checked(&current, &undo);
        fixture.checked(&current, &undo);
        assert_eq!(fs::read(&wrapper).unwrap(), old.as_bytes());
        fixture.start();
        let mut returned = Peer::connect(&listener).await;
        let returned_pid: u32 = fixture
            .launchctl(&["print", &fixture.target])
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid = "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(process_path(returned_pid), old_version.join("node"));
        assert_eq!(
            returned
                .call("read_file", json!({"root":"work","path":"handoff.txt"}))
                .await["content"],
            "native identity"
        );
        fixture.stop(&[returned_pid]).await;
        assert_eq!(fs::read(&fixture.config).unwrap(), config);
        assert_eq!(fs::metadata(&fixture.config).unwrap().ino(), config_inode);
        assert_eq!(fs::read(&policy).unwrap(), policy_bytes);
        assert_eq!(fs::read(&sidecar).unwrap(), startup);
        assert_eq!(fs::read(&fixture.plist).unwrap(), plist.as_bytes());
        assert_eq!(fixture.boot_setting(), enabled);
    }
}
