#![cfg(all(windows, feature = "experimental-windows-private-storage"))]

#[path = "support/binding.rs"]
mod binding;
#[path = "support/initial_binary.rs"]
mod initial_binary;
#[path = "support/windows.rs"]
mod support;

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use rdh_agent::{
    config::{DeviceConfig, local_roots, sha256_hex},
    random,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

struct Hub {
    origin: String,
    roots: Value,
    token_hash: Mutex<Option<String>>,
    starts: AtomicUsize,
}
async fn start(State(hub): State<Arc<Hub>>, Json(value): Json<Value>) -> (StatusCode, Json<Value>) {
    assert_eq!(value["roots"], hub.roots);
    assert!(value.get("token").is_none());
    *hub.token_hash.lock().await = value["token_hash"].as_str().map(str::to_owned);
    hub.starts.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::CREATED,
        Json(
            json!({"verification_uri_complete":format!("{}/auth/pair?code=NATIVE",hub.origin),"device_id":"native-windows-pairing","user_code":"NATIVE","device_code":"windows-native","expires_in":600,"interval":3}),
        ),
    )
}
async fn poll(State(hub): State<Arc<Hub>>, Json(value): Json<Value>) -> Json<Value> {
    assert_eq!(value["device_code"], "windows-native");
    Json(
        json!({"status":"approved","config":{"gateway_url":hub.origin,"device_id":"native-windows-pairing","roots":hub.roots,"allow_write":true,"allow_shell":true}}),
    )
}
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_windows_cli_pairs_and_reuses_private_identity_without_node() {
    let directory = std::env::temp_dir().join(format!(
        "rdh pairing '😀-{}",
        random::hex_token::<12>().unwrap()
    ));
    support::private_directory(&directory);
    let fixture = Directory(directory);
    let root = fixture.0.join("work");
    fs::create_dir(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let hub = Arc::new(Hub {
        origin: origin.clone(),
        roots: json!(local_roots(&[format!("work={}", root.display())]).unwrap()),
        token_hash: Mutex::new(None),
        starts: AtomicUsize::new(0),
    });
    let router = Router::new()
        .route("/api/pair/start", post(start))
        .route("/api/pair/poll", post(poll))
        .route(
            "/api/agent/unbind",
            post(|Json(value): Json<Value>| async move {
                Json(json!({"state":"unbound","device_id":value["device_id"]}))
            }),
        )
        .with_state(hub.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let token = random::url_token::<32>().unwrap();
    let pending = reqwest::Client::new()
        .post(format!("{origin}/api/pair/start"))
        .json(&json!({"roots":hub.roots,"token_hash":sha256_hex(token.as_bytes())}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let config = binding::seed(&fixture.0, &origin, hub.roots.clone(), &token, pending);
    let run = || {
        let mut command = support::command(initial_binary::path(), &fixture.0);
        command.env("SSH_CONNECTION", "fixture");
        command
    };
    let log_path = fixture.0.join("agent.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut child = run()
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(&log_path).unwrap()
            );
            // The atomic rename publishes the name before the writer closes
            // its exclusive Windows handle. Wait for readable bytes, not just
            // for the directory entry, without masking other I/O failures.
            match fs::read(&config) {
                Ok(bytes) => break bytes,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {}
                Err(error) => panic!("Cannot read the created binding: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let native = DeviceConfig::read(&config).unwrap();
    assert_eq!(native.device_id, "native-windows-pairing");
    assert_eq!(
        Some(sha256_hex(native.token.as_bytes())),
        *hub.token_hash.lock().await
    );
    assert_eq!(
        native.roots,
        local_roots(&[format!("work={}", root.display())]).unwrap()
    );
    assert!(
        !fs::read_to_string(&log_path)
            .unwrap()
            .contains(&native.token)
    );
    assert!(
        native
            .shell
            .to_ascii_lowercase()
            .ends_with(r"\system32\cmd.exe")
    );
    let reused = tokio::time::timeout(Duration::from_secs(15), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        reused.status.success(),
        "{}",
        String::from_utf8_lossy(&reused.stderr)
    );
    assert_eq!(fs::read(&config).unwrap(), first);
    assert_eq!(hub.starts.load(Ordering::SeqCst), 1);
    assert!(String::from_utf8_lossy(&reused.stdout).contains("Agent is already running"));
    assert!(!String::from_utf8_lossy(&reused.stdout).contains("Pairing code:"));
    let unbound = tokio::time::timeout(Duration::from_secs(30), run().arg("unbind").output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        unbound.status.success(),
        "{}",
        String::from_utf8_lossy(&unbound.stderr)
    );
    let stopped = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        stopped.success(),
        "Agent exited with {stopped} after unbind:\n{}",
        fs::read_to_string(&log_path).unwrap()
    );
    assert!(!config.exists());
    assert!(!PathBuf::from(format!("{}.rdh-lock", config.display())).exists());
    server.abort();
    let _ = server.await;
}
