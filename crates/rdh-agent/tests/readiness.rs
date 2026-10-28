#![cfg(target_os = "linux")]

use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rdh_agent::{config::sha256_hex, random, storage};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Hub {
    token: String,
    connected: mpsc::UnboundedSender<()>,
    ready: Semaphore,
    disconnect: Semaphore,
}

async fn agent(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(format!("Bearer {}", hub.token).as_str())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade
        .on_upgrade(move |mut socket| async move {
            let Some(Ok(Message::Text(hello))) = socket.recv().await else {
                return;
            };
            if serde_json::from_str::<Value>(&hello)
                .ok()
                .is_none_or(|v| v["type"] != "hello")
            {
                return;
            }
            let _ = hub.connected.send(());
            let Ok(permit) = hub.ready.acquire().await else {
                return;
            };
            permit.forget();
            if socket
                .send(Message::Text(json!({"type":"ready"}).to_string().into()))
                .await
                .is_err()
            {
                return;
            }
            loop {
                tokio::select! {
                    permit = hub.disconnect.acquire() => {
                        if let Ok(permit) = permit { permit.forget(); }
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    },
                    message = socket.recv() => if message.is_none() { return; },
                }
            }
        })
        .into_response()
}

fn command(binary: &Path, root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command
        .env_clear()
        .env("PATH", root.join("empty"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"));
    command
}

async fn probe(binary: &Path, root: &Path, config: &Path) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(5),
        command(binary, root)
            .args(["upgrade", "--probe-profile", config.to_str().unwrap()])
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

async fn wait_for_state(binary: &Path, root: &Path, config: &Path, ready: bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let output = probe(binary, root, config).await;
            if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
                if value["ready"] == ready {
                    assert_eq!(output.status.success(), ready);
                    return value;
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("Agent readiness state did not arrive")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_tracks_connections_and_new_versions_and_rejects_stale_or_forged_peers() {
    let temp = Temporary(
        std::env::temp_dir().join(format!("rdh-ready-{}", random::hex_token::<8>().unwrap())),
    );
    std::fs::create_dir(&temp.0).unwrap();
    std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["empty", "home", "work"] {
        std::fs::create_dir(temp.0.join(name)).unwrap();
    }
    let prefix = temp.0.join("install");
    std::fs::create_dir(&prefix).unwrap();
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o700)).unwrap();
    let version = prefix.join(format!("versions/{}", env!("CARGO_PKG_VERSION")));
    std::fs::create_dir_all(&version).unwrap();
    std::fs::set_permissions(
        prefix.join("versions"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&version, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_rdh"), version.join("rdh")).unwrap();
    std::fs::set_permissions(version.join("rdh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut candidate_bytes = std::fs::read(version.join("rdh")).unwrap();
    storage::write_private_new(
        &version.join(".native-version.json"),
        &serde_json::to_vec(
            &json!({"schema":1,"version":env!("CARGO_PKG_VERSION"),"kind":"native-binary",
            "binary":{"sha256":sha256_hex(&candidate_bytes),"bytes":candidate_bytes.len()}}),
        )
        .unwrap(),
    )
    .unwrap();
    symlink(
        format!("versions/{}", env!("CARGO_PKG_VERSION")),
        prefix.join("current"),
    )
    .unwrap();
    storage::write_private_new(
        &prefix.join(".native-install.json"),
        br#"{"schema":1,"kind":"native-agent"}"#,
    )
    .unwrap();
    storage::write_private_new(
        &prefix.join("install-id"),
        format!("{}\n", "a".repeat(32)).as_bytes(),
    )
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let token = "s".repeat(32);
    let (connections, mut connected) = mpsc::unbounded_channel();
    let hub = Arc::new(Hub {
        token: token.clone(),
        connected: connections,
        ready: Semaphore::new(0),
        disconnect: Semaphore::new(0),
    });
    let router = Router::new()
        .route("/agent/{id}", get(agent))
        .with_state(Arc::clone(&hub));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let config = temp.0.join("device.json");
    storage::write_private_new(
        &config,
        &serde_json::to_vec(&json!({"gateway_url":origin,
        "device_id":"fixture", "token":token, "roots":{"work":temp.0.join("work")}}))
        .unwrap(),
    )
    .unwrap();
    let binary = prefix.join("current/rdh");
    let mut child = command(&binary, &temp.0)
        .args([
            "--server",
            &origin,
            "--config",
            config.to_str().unwrap(),
            "--no-browser",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), connected.recv())
        .await
        .unwrap()
        .unwrap();
    let offline = wait_for_state(&binary, &temp.0, &config, false).await;
    assert_eq!(offline["pid"], child.id().unwrap());
    assert!(offline["connection_generation"].is_null());

    hub.ready.add_permits(1);
    let first = wait_for_state(&binary, &temp.0, &config, true).await;
    assert_eq!(first["device_id"], "fixture");
    assert_eq!(first["version"], env!("CARGO_PKG_VERSION"));
    assert!(!first.to_string().contains(&token));
    let generation = first["connection_generation"].as_u64().unwrap();
    hub.disconnect.add_permits(1);
    let disconnected = wait_for_state(&binary, &temp.0, &config, false).await;
    assert_eq!(disconnected["instance_id"], first["instance_id"]);
    tokio::time::timeout(Duration::from_secs(10), connected.recv())
        .await
        .unwrap()
        .unwrap();
    hub.ready.add_permits(1);
    let second = wait_for_state(&binary, &temp.0, &config, true).await;
    assert_eq!(second["instance_id"], first["instance_id"]);
    assert!(second["connection_generation"].as_u64().unwrap() > generation);

    // This fixture shares its cgroup with the test process. It must never stop
    // the Agent on the strength of an idle job registry alone.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let output = command(&binary, &temp.0)
                .args(["upgrade", "--stop-if-idle", config.to_str().unwrap()])
                .output()
                .await
                .unwrap();
            assert!(
                !output.status.success(),
                "a shared cgroup must not be stopped"
            );
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("cgroup is not isolated") {
                break;
            }
            assert!(error.contains("active or undelivered"), "{error}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        wait_for_state(&binary, &temp.0, &config, true).await["ready"]
            .as_bool()
            .unwrap()
    );

    // Ambiguous or stale markers cannot be silently selected as readiness.
    let duplicate_nonce = random::hex_token::<16>().unwrap();
    let duplicate = prefix
        .join(".running")
        .join(format!("{}-{duplicate_nonce}.json", i32::MAX));
    storage::write_private_new(
        &duplicate,
        &serde_json::to_vec(&json!({"schema":1,"pid":i32::MAX,
        "nonce":duplicate_nonce,"config":config}))
        .unwrap(),
    )
    .unwrap();
    let ambiguous = probe(&binary, &temp.0, &config).await;
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("Multiple native running markers"));
    std::fs::remove_file(duplicate).unwrap();

    // Stop cleanly, perform the real offline update transaction, and require a
    // fresh instance of the candidate to receive Hub ready after switching.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!probe(&binary, &temp.0, &config).await.status.success());
    let old = env!("CARGO_PKG_VERSION");
    let next = old.replace("rust.1", "rust.2");
    assert_ne!(old, next);
    assert_eq!(old.len(), next.len());
    let mut replacements = 0;
    let mut offset = 0;
    while offset + old.len() <= candidate_bytes.len() {
        if &candidate_bytes[offset..offset + old.len()] == old.as_bytes() {
            candidate_bytes[offset..offset + old.len()].copy_from_slice(next.as_bytes());
            replacements += 1;
            offset += old.len();
        } else {
            offset += 1;
        }
    }
    assert!(replacements > 0);
    std::fs::write(temp.0.join("candidate"), &candidate_bytes).unwrap();
    let platform = match (std::env::consts::ARCH, cfg!(target_env = "musl")) {
        ("x86_64", false) => "linux-x64-gnu",
        ("x86_64", true) => "linux-x64-musl",
        ("aarch64", false) => "linux-arm64-gnu",
        ("aarch64", true) => "linux-arm64-musl",
        _ => panic!("unsupported test architecture"),
    };
    let index = temp.0.join("channel.json");
    std::fs::write(
        &index,
        serde_json::to_vec(&json!({"schema":2,"channel":"stable","version":next,
        "platforms":{(platform):{"format":"native-binary","url":"candidate",
            "sha256":sha256_hex(&candidate_bytes),"bytes":candidate_bytes.len()}}}))
        .unwrap(),
    )
    .unwrap();
    drop(candidate_bytes);
    let update = tokio::time::timeout(
        Duration::from_secs(60),
        command(&binary, &temp.0)
            .args([
                "upgrade",
                "--index",
                index.to_str().unwrap(),
                "--no-restart",
            ])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    assert_eq!(
        std::fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{next}"))
    );
    child = command(&binary, &temp.0)
        .args([
            "--server",
            &origin,
            "--config",
            config.to_str().unwrap(),
            "--no-browser",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), connected.recv())
        .await
        .unwrap()
        .unwrap();
    let before_ready = wait_for_state(&binary, &temp.0, &config, false).await;
    assert_eq!(before_ready["version"], next);
    assert_ne!(before_ready["instance_id"], second["instance_id"]);
    hub.ready.add_permits(1);
    let upgraded = wait_for_state(&binary, &temp.0, &config, true).await;
    assert_eq!(upgraded["version"], next);
    assert_eq!(upgraded["pid"], child.id().unwrap());
    assert_eq!(upgraded["instance_id"], before_ready["instance_id"]);

    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let marker_path = std::fs::read_dir(prefix.join(".running"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let marker: Value = serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
    assert!(!probe(&binary, &temp.0, &config).await.status.success());
    // Use a different live process as the recorded PID so pidfd succeeds; the
    // socket is owned by this test process and must still fail peer identity.
    std::fs::remove_file(prefix.join(".running").join(format!(
        "{}-{}.json",
        marker["pid"].as_u64().unwrap(),
        marker["nonce"].as_str().unwrap()
    )))
    .unwrap();
    let mut unrelated = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let fake_pid = unrelated.id().unwrap();
    let fake_nonce = random::hex_token::<16>().unwrap();
    let fake_marker = json!({"schema":1,"pid":fake_pid,"nonce":fake_nonce,"config":config});
    storage::write_private_new(
        &prefix
            .join(".running")
            .join(format!("{fake_pid}-{fake_nonce}.json")),
        &serde_json::to_vec(&fake_marker).unwrap(),
    )
    .unwrap();
    let address = std::os::unix::net::SocketAddr::from_abstract_name(
        format!("rdh-ready-{fake_pid}-{fake_nonce}").as_bytes(),
    )
    .unwrap();
    let forged = tokio::net::UnixListener::bind_addr(&address.into()).unwrap();
    let result = probe(&binary, &temp.0, &config).await;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("peer does not match"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    drop(forged);
    unrelated.kill().await.unwrap();
    unrelated.wait().await.unwrap();
    server.abort();
}
