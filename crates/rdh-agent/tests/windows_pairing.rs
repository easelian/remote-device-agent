#![cfg(all(windows, feature = "experimental-windows-private-storage"))]

#[path = "support/initial_binary.rs"]
mod initial_binary;
#[path = "support/windows.rs"]
mod support;

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use rdh_agent::{
    config::{DeviceConfig, local_roots, sha256_hex},
    profile_lock::ProfileLocks,
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
            json!({"verification_uri_complete":format!("{}/admin/pair?code=NATIVE",hub.origin),"user_code":"NATIVE","device_code":"windows-native","expires_in":600,"interval":3}),
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
    let config = fixture.0.join("device '😀.json");
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
        .with_state(hub.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let args = vec![
        "--server".to_owned(),
        origin,
        "--config".into(),
        config.to_str().unwrap().to_owned(),
        "--root".into(),
        format!("work={}", root.display()),
        "--pair-only".into(),
        "--no-browser".into(),
    ];
    let run = || {
        let mut command = support::command(initial_binary::path(), &fixture.0);
        command.args(&args);
        command
    };
    let paired = tokio::time::timeout(Duration::from_secs(30), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        paired.status.success(),
        "{}",
        String::from_utf8_lossy(&paired.stderr)
    );
    let first = fs::read(&config).unwrap();
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
    assert!(!String::from_utf8_lossy(&paired.stdout).contains(&native.token));
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
    let canonical = config.canonicalize().unwrap();
    let mut held = ProfileLocks::acquire(std::slice::from_ref(&canonical)).unwrap();
    let locked = tokio::time::timeout(Duration::from_secs(15), run().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!locked.status.success());
    assert_eq!(fs::read(&config).unwrap(), first);
    held.release().unwrap();
    assert_eq!(hub.starts.load(Ordering::SeqCst), 1);
    assert!(
        !config
            .with_file_name(format!(
                "{}.node-lock",
                config.file_name().unwrap().to_string_lossy()
            ))
            .exists()
    );
    server.abort();
    let _ = server.await;
}
