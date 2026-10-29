//! Real old Node task -> Rust -> Node, with both installer/profile layouts.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};

const TOKEN: &str = "windows-legacy-task-fixture-token-only";
fn local(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(path.to_str().unwrap())
        .to_owned()
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace("'", "''"))
}

struct Peer {
    socket: WebSocketStream<TcpStream>,
    next: u64,
    receive_budget: Duration,
}
impl Peer {
    async fn connect(listener: &TcpListener, task: &windows_tasks::Task, legacy: bool) -> Self {
        // The unchanged Node startup performs several sequential ACL checks,
        // each with its own 60 s PowerShell limit. Native ARM64 evidence for
        // those old calls is recorded in legacy_lock_interop.rs. Keep a bounded
        // composition deadline here; the native connection limit stays 30 s.
        let budget = Duration::from_secs(if legacy { 900 } else { 30 });
        let started = std::time::Instant::now();
        let (stream, _) = tokio::time::timeout(budget, async {
            loop {
                tokio::select! {
                    accepted=listener.accept()=>break accepted,
                    _=tokio::time::sleep(Duration::from_secs(2))=>{
                        let (active,result)=task.execution_state();
                        assert!(started.elapsed()<Duration::from_secs(5)||active,
                            "Task exited before connecting: last_result={result}; inspect the retained fixture logs");
                    }
                }
            }
        })
            .await
            .expect("Legacy/native task did not connect")
            .unwrap();
        let socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/agent/windows-legacy-task");
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
            next: 0,
            receive_budget: if legacy {
                budget
            } else {
                Duration::from_secs(20)
            },
        };
        let hello = peer.receive().await;
        assert_eq!(hello["type"], "hello");
        peer.send(json!({"type":"ready","device_id":"windows-legacy-task"}))
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
        tokio::time::timeout(self.receive_budget, async {
            loop {
                match self.socket.next().await.unwrap().unwrap() {
                    Message::Text(s) => return serde_json::from_str(&s).unwrap(),
                    Message::Ping(data) => self.socket.send(Message::Pong(data)).await.unwrap(),
                    Message::Pong(_) => {}
                    m => panic!("Unexpected Task session message: {m:?}"),
                }
            }
        })
        .await
        .expect("Task RPC timed out")
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
fn process(pid: u32, expected: &Path) -> OwnedHandle {
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    assert!(
        !handle.is_null(),
        "OpenProcess: {}",
        std::io::Error::last_os_error()
    );
    let process = unsafe { OwnedHandle::from_raw_handle(handle) };
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle(), 0) },
        WAIT_TIMEOUT
    );
    let mut image = vec![0u16; 32768];
    let mut size = image.len() as u32;
    assert_ne!(
        unsafe {
            QueryFullProcessImageNameW(process.as_raw_handle(), 0, image.as_mut_ptr(), &mut size)
        },
        0
    );
    assert_eq!(
        Path::new(&String::from_utf16(&image[..size as usize]).unwrap())
            .canonicalize()
            .unwrap(),
        expected.canonicalize().unwrap()
    );
    process
}
fn legacy_process(config: &Path, node: &Path) -> OwnedHandle {
    let owner: Value = serde_json::from_slice(
        &fs::read(PathBuf::from(format!("{}.node-lock", config.display())).join("owner.json"))
            .unwrap(),
    )
    .unwrap();
    process(u32::try_from(owner["pid"].as_u64().unwrap()).unwrap(), node)
}
fn file_identity(path: &Path) -> (u32, u32, u32) {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = fs::File::open(path).unwrap();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    assert_ne!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) },
        0
    );
    (
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the actual legacy build, native artifact and disposable Windows Task Scheduler"]
async fn legacy_windows_task_handoff_preserves_identity_and_native_update_path() {
    let source = PathBuf::from(
        std::env::var_os("RDH_LEGACY_SOURCE").expect("Build the actual legacy source first"),
    );
    let source_node = PathBuf::from(
        std::env::var_os("RDH_LEGACY_NODE").expect("Use the native-architecture legacy Node"),
    );
    let package: Value =
        serde_json::from_slice(&fs::read(source.join("package.json")).unwrap()).unwrap();
    let legacy_version = package["version"].as_str().unwrap();
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "arm64"
    };
    let (next, candidate) = native_candidate::load();
    for installer in [true, false] {
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "rdh legacy task ' 😀-{}",
            random::hex_token::<8>().unwrap()
        )));
        windows::private_directory(&fixture.0);
        let root = fixture.0.canonicalize().unwrap();
        let legacy = root.join("legacy program");
        windows::private_directory(&legacy);
        let versions = legacy.join("versions");
        windows::private_directory(&versions);
        let directory = versions.join(format!("rdh-{legacy_version}-win32-{arch}"));
        windows::private_directory(&directory);
        for name in ["bin", "bundle"] {
            windows::private_directory(&directory.join(name));
        }
        let node = directory.join("node.exe");
        fs::copy(&source_node, &node).unwrap();
        for name in [
            "package.json",
            "bin/remote-device-hub.mjs",
            "bundle/agent.mjs",
        ] {
            fs::copy(source.join(name), directory.join(name)).unwrap();
        }
        let runner = directory.join("windows-logon.ps1");
        fs::copy(source.join("packaging/windows-logon.ps1"), &runner).unwrap();
        let data = if installer {
            legacy.join("data")
        } else {
            root.join("profile")
        };
        windows::private_directory(&data);
        let config = data.join("agent.json");
        let policy = config.with_extension("policy.json");
        let work = root.join("work");
        windows::private_directory(&work);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        storage::write_private_new(&config,&serde_json::to_vec(&json!({"gateway_url":origin,"device_id":"windows-legacy-task","token":TOKEN,"roots":local_roots(&[format!("work={}",work.display())]).unwrap(),"allow_shell":true,"allow_write":true})).unwrap()).unwrap();
        storage::write_private_new(&policy, br#"{"version":1,"audit":{"enabled":false}}"#).unwrap();
        let cwd = if installer {
            local(&legacy)
        } else {
            local(&data)
        };
        let log = if installer {
            data.join("agent.log")
        } else {
            PathBuf::from(format!("{}.startup.log", local(&config)))
        };
        let args = [
            local(&directory.join("bin/remote-device-hub.mjs")),
            "--config".into(),
            local(&config),
            "--server".into(),
            origin.clone(),
            "--no-browser".into(),
            "--recover-lock".into(),
        ];
        let command = format!(
            "& {} -Node {} -Log {} -AgentArguments @({})",
            literal(&local(&runner)),
            literal(&local(&node)),
            literal(&local(&log)),
            args.iter()
                .map(|s| literal(s))
                .collect::<Vec<_>>()
                .join(",")
        );
        let arguments = format!(
            "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}",
            STANDARD.encode(
                command
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            )
        );
        let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let sid = windows::current_sid();
        let mut task = windows_tasks::Task::create(&sid, &local(&powershell), &arguments, &cwd);
        let sidecar = PathBuf::from(format!("{}.startup.json", config.display()));
        storage::write_private_new(&sidecar,&serde_json::to_vec(&json!({"version":1,"binding":{"mode":"user","name":task.name},"pending":{"request_id":"55555555-5555-4555-8555-555555555555","mode":"user","expires_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64+24*60*60*1000}})).unwrap()).unwrap();
        let config_bytes = fs::read(&config).unwrap();
        let config_id = file_identity(&config);
        let policy_bytes = fs::read(&policy).unwrap();
        let sidecar_bytes = fs::read(&sidecar).unwrap();
        let prefix = root.join("install");
        success(
            run(
                initial_binary::path(),
                &root,
                &[
                    "install-native",
                    "--prefix",
                    prefix.to_str().unwrap(),
                    "--config",
                    config.to_str().unwrap(),
                ],
            )
            .await,
        );
        let initial = binary(&prefix, env!("CARGO_PKG_VERSION"));
        task.set_enabled(true);
        let original_task = task.inspect();
        let fingerprint = task.migration_fingerprint();
        let security = task.security();
        let apply_owned = [
            "migrate".into(),
            "--offline".into(),
            "--prefix".into(),
            local(&legacy),
            "--config".into(),
            local(&config),
            "--service-unit".into(),
            task.name.clone(),
            "--recover-lock".into(),
        ];
        let apply: Vec<&str> = apply_owned.iter().map(String::as_str).collect();
        let mut undo_owned = apply_owned.clone();
        undo_owned[1] = "--undo".into();
        let undo: Vec<&str> = undo_owned.iter().map(String::as_str).collect();
        task.start();
        let mut old_peer = Peer::connect(&listener, &task, true).await;
        let old_process = legacy_process(&config, &node);
        old_peer
            .call(
                "write_file",
                json!({"root":"work","path":"handoff.txt","content":"original task identity"}),
            )
            .await;
        assert!(!run(&initial, &root, &apply).await.status.success());
        task.stop();
        stopped_task_agent(&old_process);
        drop(old_peer);
        success(run(&initial, &root, &apply).await);
        success(run(&initial, &root, &apply).await);
        assert_eq!(task.inspect().0, local(&initial));
        assert_eq!(task.inspect().2, cwd);
        assert_eq!(task.migration_fingerprint(), fingerprint);
        assert_eq!(task.security(), security);
        task.set_enabled(false);
        assert!(!run(&initial, &root, &undo).await.status.success());
        task.set_enabled(true);
        // Reconstruct interruption after the durable record but before the
        // one COM publication, then recover without replacing unrelated state.
        task.replace_action(&sid, &original_task.0, &original_task.1);
        success(run(&initial, &root, &apply).await);
        let original_runner = fs::read(&runner).unwrap();
        fs::write(&runner, b"externally changed wrapper").unwrap();
        assert!(!run(&initial, &root, &undo).await.status.success());
        fs::write(&runner, &original_runner).unwrap();
        task.start();
        let mut native_peer = Peer::connect(&listener, &task, false).await;
        let native_process = running_task_agent(&prefix, &initial, &config).await;
        assert_eq!(
            native_peer
                .call("read_file", json!({"root":"work","path":"handoff.txt"}))
                .await["content"],
            "original task identity"
        );
        task.stop();
        stopped_task_agent(&native_process);
        drop(native_peer);
        // Offline native updates must retain the old installer's cwd, including
        // the installer case where it differs from the configuration directory.
        fs::write(root.join("candidate.exe"), &candidate).unwrap();
        let index = root.join("channel.json");
        fs::write(&index,serde_json::to_vec(&json!({"schema":2,"channel":"stable","version":next,"platforms":{(format!("win32-{arch}")):{"format":"native-binary","url":"candidate.exe","sha256":sha256_hex(&candidate),"bytes":candidate.len()}}})).unwrap()).unwrap();
        success(
            run(
                &initial,
                &root,
                &[
                    "upgrade",
                    "--index",
                    index.to_str().unwrap(),
                    "--no-restart",
                    "--recover-lock",
                ],
            )
            .await,
        );
        let next_binary = binary(&prefix, &next);
        assert_eq!(task.inspect().0, local(&next_binary));
        assert_eq!(task.inspect().2, cwd);
        assert_eq!(task.migration_fingerprint(), fingerprint);
        task.start();
        let mut next_peer = Peer::connect(&listener, &task, false).await;
        let next_process = running_task_agent(&prefix, &next_binary, &config).await;
        assert_eq!(
            next_peer
                .call("read_file", json!({"root":"work","path":"handoff.txt"}))
                .await["content"],
            "original task identity"
        );
        task.stop();
        stopped_task_agent(&next_process);
        drop(next_peer);
        success(
            run(
                &next_binary,
                &root,
                &["rollback", "--no-restart", "--recover-lock"],
            )
            .await,
        );
        success(run(&initial, &root, &undo).await);
        success(run(&initial, &root, &undo).await);
        let restored = task.inspect();
        assert_eq!(
            (restored.0, restored.1, restored.2, restored.3),
            (
                original_task.0,
                original_task.1,
                original_task.2,
                original_task.3
            )
        );
        task.start();
        let mut restored_peer = Peer::connect(&listener, &task, true).await;
        let restored_process = legacy_process(&config, &node);
        assert_eq!(
            restored_peer
                .call("read_file", json!({"root":"work","path":"handoff.txt"}))
                .await["content"],
            "original task identity"
        );
        task.stop();
        stopped_task_agent(&restored_process);
        drop(restored_peer);
        drop(
            install_lock::ManagerLock::acquire_with_stale_recovery(&prefix, &initial, false, true)
                .unwrap(),
        );
        assert_eq!(fs::read(&config).unwrap(), config_bytes);
        assert_eq!(file_identity(&config), config_id);
        assert_eq!(fs::read(&policy).unwrap(), policy_bytes);
        assert_eq!(fs::read(&sidecar).unwrap(), sidecar_bytes);
        assert_eq!(task.security(), security);
        assert_eq!(task.migration_fingerprint(), fingerprint);
        task.remove();
    }
}
