//! Exercise crash recovery with the real CLI and isolated binding/state paths.
#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
))]
#[path = "support/initial_binary.rs"]
mod initial_binary;
#[cfg(windows)]
#[allow(dead_code)]
#[path = "support/windows.rs"]
mod windows;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use rdh_agent::{
    config::{DEFAULT_SERVER, DeviceConfig, ProfilePaths, local_roots},
    storage,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::{Child, Command};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
fn command(base: &Path) -> Command {
    let mut command = Command::new(initial_binary::path());
    command
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("XDG_STATE_HOME", base.join("state"))
        .env("XDG_CACHE_HOME", base.join("cache"))
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .env_remove("RDH_USE_ENV_PROXY")
        .env_remove("RDH_CA_BUNDLE")
        .env_remove("RDH_SERVER")
        .env_remove("RDH_INSTALL_DIR")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}
async fn start(base: &Path, config: &Path, name: &str) -> Child {
    let path = base.join(format!("{name}.log"));
    let log = std::fs::File::create(&path).unwrap();
    let mut child = command(base)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "{}",
                std::fs::read_to_string(&path).unwrap()
            );
            if let Ok(raw) = std::fs::read(sidecar(config, ".instance.json")) {
                let value: Value = serde_json::from_slice(&raw).unwrap();
                if value["pid"] == pid && value["status"]["state"] == "bound" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Agent did not become bound");
    child
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_binding_recovers_without_repair_and_unbind_keeps_its_intent() {
    let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-lifecycle-{}",
        rdh_agent::random::hex_token::<12>().unwrap()
    ));
    #[cfg(unix)]
    storage::create_private_dir(&base).unwrap();
    #[cfg(windows)]
    windows::private_directory(&base);
    let fixture = Fixture(base.clone());
    let root = base.join("work");
    std::fs::create_dir(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let allow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let service = Router::new()
        .route(
            "/api/agent/unbind",
            post(
                |State(allow): State<std::sync::Arc<std::sync::atomic::AtomicBool>>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    assert_eq!(
                        headers["authorization"],
                        "Bearer isolated-lifecycle-token-for-native-tests-1234"
                    );
                    assert_eq!(body["device_id"], "lifecycle-device");
                    if !allow.load(std::sync::atomic::Ordering::Acquire) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error":"injected"})),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({"device_id":"lifecycle-device","state":"unbound"})),
                    )
                },
            ),
        )
        .with_state(allow.clone());
    let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let config = ProfilePaths::resolve(&base.join("config"), DEFAULT_SERVER, "default", None)
        .unwrap()
        .destination;
    storage::ensure_parent(&config).unwrap();
    DeviceConfig {
        gateway_url: origin,
        discovery_url: None,
        device_id: "lifecycle-device".into(),
        token: "isolated-lifecycle-token-for-native-tests-1234".into(),
        roots: local_roots(&[format!("work={}", root.display())]).unwrap(),
        allow_write: false,
        allow_shell: false,
        shell: if cfg!(windows) {
            "powershell.exe"
        } else {
            "/bin/sh"
        }
        .into(),
        max_processes: 1,
        max_process_seconds: 10,
    }
    .write_new(&config)
    .unwrap();
    let original = storage::read_private(&config).unwrap();
    let mut first = start(&base, &config, "first").await;
    first.kill().await.unwrap();
    first.wait().await.unwrap();
    assert!(sidecar(&config, ".rdh-lock").exists());
    let mut second = start(&base, &config, "restarted").await;
    let before = std::fs::read(sidecar(&config, ".rdh-lock").join("owner.json")).unwrap();
    let duplicate = tokio::time::timeout(Duration::from_secs(5), command(&base).output())
        .await
        .unwrap()
        .unwrap();
    assert!(duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stdout).contains("already running"));
    assert_eq!(
        std::fs::read(sidecar(&config, ".rdh-lock").join("owner.json")).unwrap(),
        before
    );
    assert!(second.try_wait().unwrap().is_none());
    second.kill().await.unwrap();
    second.wait().await.unwrap();
    let mut record: Value =
        serde_json::from_slice(&std::fs::read(sidecar(&config, ".instance.json")).unwrap())
            .unwrap();
    record["pid"] = json!(std::process::id());
    record["started"] = json!("earlier-birth-of-reused-pid");
    storage::write_private_replace(
        &sidecar(&config, ".instance.json"),
        &serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let mut third = start(&base, &config, "reused-pid").await;
    assert_eq!(storage::read_private(&config).unwrap(), original);
    third.kill().await.unwrap();
    third.wait().await.unwrap();
    let unbind = tokio::time::timeout(
        Duration::from_secs(8),
        command(&base).arg("unbind").output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!unbind.status.success());
    let error = String::from_utf8_lossy(&unbind.stderr);
    assert!(error.contains("Hub revocation"), "{error}");
    assert!(!error.contains("shutdown is not confirmed"), "{error}");
    assert!(sidecar(&config, ".unbind.json").exists());
    assert_eq!(storage::read_private(&config).unwrap(), original);
    allow.store(true, std::sync::atomic::Ordering::Release);
    let completed = tokio::time::timeout(
        Duration::from_secs(8),
        command(&base).arg("unbind").output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        completed.status.success(),
        "{}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert!(
        !config.exists()
            && !sidecar(&config, ".unbind.json").exists()
            && !sidecar(&config, ".rdh-lock").exists()
    );
    server.abort();
    let _ = server.await;
    drop(fixture);
}
