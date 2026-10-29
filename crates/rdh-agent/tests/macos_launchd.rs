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
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
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
        let _ = fs::remove_dir_all(&self.directory);
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
