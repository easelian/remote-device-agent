#![cfg(target_os = "linux")]

use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rdh_agent::install_lock::{ControllerLock, LaunchBarrier, ManagerLock, RunningInstall};
use rdh_agent::systemd_identity::UnitIdentity;
use rdh_agent::{random, service_update, storage};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Peer {
    hello: Value,
    requests: mpsc::UnboundedSender<Value>,
    replies: mpsc::UnboundedReceiver<Value>,
}
struct Hub {
    token: String,
    peers: mpsc::UnboundedSender<Peer>,
    ready: Semaphore,
}

async fn agent(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some(format!("Bearer {}", hub.token).as_str())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade
        .on_upgrade(move |mut socket| async move {
            let Some(Ok(Message::Text(hello))) = socket.recv().await else {
                return;
            };
            let Ok(hello) = serde_json::from_str(&hello) else {
                return;
            };
            let (requests, mut request_rx) = mpsc::unbounded_channel::<Value>();
            let (reply_tx, replies) = mpsc::unbounded_channel();
            if hub
                .peers
                .send(Peer {
                    hello,
                    requests,
                    replies,
                })
                .is_err()
            {
                return;
            }
            let Ok(permit) = hub.ready.acquire().await else { return; };
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
                    request = request_rx.recv() => match request {
                        Some(request) if socket.send(Message::Text(request.to_string().into())).await.is_ok() => {},
                        _ => return,
                    },
                    message = socket.recv() => match message {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(value) = serde_json::from_str(&text) {
                                let _ = reply_tx.send(value);
                            }
                        },
                        Some(Ok(_)) => {},
                        _ => return,
                    }
                }
            }
        })
        .into_response()
}

fn command(binary: &Path, root: &Path) -> tokio::process::Command {
    let mut result = tokio::process::Command::new(binary);
    result
        .env_clear()
        .env("PATH", root.join("empty"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"));
    result
}

async fn rejected_launch(binary: &Path, root: &Path, args: &[&str]) -> String {
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        command(binary, root).args(args).output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!result.status.success());
    String::from_utf8_lossy(&result.stderr).into_owned()
}

async fn snapshot(binary: &Path, root: &Path, config: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let output = command(binary, root)
                .args(["upgrade", "--probe-profile", config.to_str().unwrap()])
                .output()
                .await
                .unwrap();
            if output.status.success() {
                return serde_json::from_slice(&output.stdout).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("trial did not become ready")
}

async fn activate(marker: &Value, instance: &str) -> u8 {
    let name = format!(
        "rdh-activate-{}-{}",
        marker["pid"].as_u64().unwrap(),
        marker["nonce"].as_str().unwrap()
    );
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let mut socket = tokio::net::UnixStream::connect_addr(&address.into())
        .await
        .unwrap();
    socket.write_all(instance.as_bytes()).await.unwrap();
    let mut response = [0];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut response))
        .await
        .unwrap()
        .unwrap();
    response[0]
}

fn marker(prefix: &Path) -> Value {
    let entries: Vec<_> = std::fs::read_dir(prefix.join(".running"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    serde_json::from_slice(&storage::read_private(&entries[0]).unwrap()).unwrap()
}

async fn request(peer: &mut Peer, id: &str, path: &str) -> Value {
    peer.requests
        .send(json!({"type":"request","id":id,"operation":"write_file",
            "params":{"root":"work","path":path,"content":"created after acceptance"}}))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = peer.replies.recv().await.unwrap();
            if response["type"] == "result" && response["id"] == id {
                return response;
            }
        }
    })
    .await
    .unwrap()
}

fn proof(marker: &Value, instance: &str) -> Value {
    json!({"pid":marker["pid"],"nonce":marker["nonce"],"instance_id":instance})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trial_refuses_work_until_a_ready_exact_instance_is_committed_and_activated() {
    let temp = Temporary(std::env::temp_dir().join(format!(
        "rdh-service-trial-{}",
        random::hex_token::<8>().unwrap()
    )));
    std::fs::create_dir(&temp.0).unwrap();
    std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["empty", "home", "work"] {
        std::fs::create_dir(temp.0.join(name)).unwrap();
    }
    let prefix = temp.0.join("install");
    let installed = command(Path::new(env!("CARGO_BIN_EXE_rdh")), &temp.0)
        .args(["install-native", "--prefix", prefix.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let binary = prefix.join("current/rdh");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let token = "s".repeat(32);
    let (peers, mut connected) = mpsc::unbounded_channel();
    let hub = Arc::new(Hub {
        token: token.clone(),
        peers,
        // First launch and restart proceed normally. Recovery below explicitly
        // controls ready delivery to cover the handshake/probe ordering.
        ready: Semaphore::new(2),
    });
    let router = Router::new()
        .route("/agent/{id}", get(agent))
        .with_state(Arc::clone(&hub));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let config = temp.0.join("device.json");
    let other = temp.0.join("other.json");
    let config_data = serde_json::to_vec(&json!({"gateway_url":origin,"device_id":"fixture",
        "token":token,"roots":{"work":temp.0.join("work")},"allow_write":true}))
    .unwrap();
    storage::write_private_new(&config, &config_data).unwrap();
    storage::write_private_new(&other, &config_data).unwrap();
    let record_path = prefix.join(service_update::RECORD);
    let mut intent = json!({"schema":1,"transaction_id":"a".repeat(32),"unit":"rdh.service",
        "config":config,"old_current":"0.7.1","old_previous":null,
        "new_current":env!("CARGO_PKG_VERSION"),"original":{"pid":1,"nonce":"d".repeat(32),"instance_id":"e".repeat(32)},
        "original_invocation":"f".repeat(32),"original_definition":"1".repeat(64),
        "phase":"preparing","proof":null});
    storage::write_private_new(&record_path, &serde_json::to_vec(&intent).unwrap()).unwrap();
    let launch_args = [
        "--server",
        origin.as_str(),
        "--config",
        config.to_str().unwrap(),
        "--no-browser",
    ];
    assert!(
        rejected_launch(&binary, &temp.0, &launch_args)
            .await
            .contains("interrupted")
    );
    assert!(
        rejected_launch(&binary, &temp.0, &["upgrade", "--check"])
            .await
            .contains("service update")
    );

    intent["phase"] = json!("trial");
    storage::write_private_replace(&record_path, &serde_json::to_vec(&intent).unwrap()).unwrap();
    let barrier = LaunchBarrier::acquire(&prefix).unwrap();
    assert!(
        rejected_launch(&binary, &temp.0, &launch_args)
            .await
            .contains("update is in progress")
    );
    drop(barrier);
    let other_args = [
        "--server",
        origin.as_str(),
        "--config",
        other.to_str().unwrap(),
        "--no-browser",
    ];
    assert!(
        rejected_launch(&binary, &temp.0, &other_args)
            .await
            .contains("recorded existing service profile")
    );
    let pair_args = [
        "--server",
        origin.as_str(),
        "--config",
        config.to_str().unwrap(),
        "--pair-only",
    ];
    assert!(
        rejected_launch(&binary, &temp.0, &pair_args)
            .await
            .contains("recorded existing service profile")
    );

    let mut first = command(&binary, &temp.0)
        .args(launch_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut peer = tokio::time::timeout(Duration::from_secs(10), connected.recv())
        .await
        .unwrap()
        .unwrap();
    let first_state = snapshot(&binary, &temp.0, &config).await;
    let instance = first_state["instance_id"].as_str().unwrap();
    assert_eq!(
        peer.hello["info"]["capabilities"]["operation_receipts"]["instance_id"],
        instance
    );
    assert_eq!(first_state["quarantined"], true);
    assert!(!first_state.to_string().contains(&token));
    let result = request(&mut peer, "before-commit", "before-commit").await;
    assert_eq!(result["execution_state"], "not_sent");
    assert!(!temp.0.join("work/before-commit").exists());
    let first_marker = marker(&prefix);
    assert_eq!(activate(&first_marker, instance).await, b'I');

    intent["phase"] = json!("commit_trial");
    intent["proof"] = proof(&first_marker, &"b".repeat(32));
    storage::write_private_replace(&record_path, &serde_json::to_vec(&intent).unwrap()).unwrap();
    assert_eq!(activate(&first_marker, instance).await, b'I');
    intent["proof"] = proof(&first_marker, instance);
    storage::write_private_replace(&record_path, &serde_json::to_vec(&intent).unwrap()).unwrap();
    assert_eq!(
        snapshot(&binary, &temp.0, &config).await["quarantined"],
        true
    );
    assert_eq!(
        request(&mut peer, "after-commit", "after-commit").await["execution_state"],
        "not_sent"
    );
    assert!(!temp.0.join("work/after-commit").exists());

    // A restart after the durable commit must not inherit the old instance's
    // activation proof. Recovery must verify the new instance explicitly.
    assert_eq!(
        unsafe { libc::kill(first.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), first.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(peer);
    let mut second = command(&binary, &temp.0)
        .args(launch_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut peer = tokio::time::timeout(Duration::from_secs(10), connected.recv())
        .await
        .unwrap()
        .unwrap();
    let second_state = snapshot(&binary, &temp.0, &config).await;
    let second_instance = second_state["instance_id"].as_str().unwrap();
    assert_ne!(second_instance, instance);
    assert_eq!(second_state["quarantined"], true);
    let second_marker = marker(&prefix);
    assert_eq!(activate(&second_marker, second_instance).await, b'I');
    assert_eq!(
        request(&mut peer, "new-instance", "new-instance").await["execution_state"],
        "not_sent"
    );
    intent["proof"] = proof(&second_marker, second_instance);
    storage::write_private_replace(&record_path, &serde_json::to_vec(&intent).unwrap()).unwrap();
    assert_eq!(activate(&second_marker, second_instance).await, b'A');
    assert_eq!(
        snapshot(&binary, &temp.0, &config).await["quarantined"],
        false
    );
    let result = request(&mut peer, "accepted", "accepted").await;
    assert!(result["result"]["sha256"].is_string(), "{result}");
    assert!(temp.0.join("work/accepted").exists());

    let barrier = LaunchBarrier::acquire(&prefix).unwrap();
    intent["phase"] = json!("accepted_trial");
    storage::write_private_replace(&record_path, &serde_json::to_vec(&intent).unwrap()).unwrap();
    std::fs::remove_file(&record_path).unwrap();
    std::fs::File::open(&prefix).unwrap().sync_all().unwrap();
    drop(barrier);

    // Exercise the serialized journal as an interrupted pre-stop update. The
    // service identity is supplied by this local test; real systemd identity
    // validation is exercised separately against a temporary unit.
    let controller = ControllerLock::acquire(&prefix, &binary).unwrap();
    assert!(ControllerLock::acquire(&prefix, &binary).is_err());
    let guard = RunningInstall::inspect_at(&prefix, &binary).unwrap();
    let barrier = LaunchBarrier::acquire(&prefix).unwrap();
    let live = rdh_agent::readiness::probe_profile(&guard, &prefix, &config)
        .await
        .unwrap();
    let identity = UnitIdentity {
        unit: "rdh.service".into(),
        pid: live.pid,
        invocation_id: "f".repeat(32),
        control_group: "/fixture/rdh.service".into(),
        restart: "no".into(),
        executable: binary.to_str().unwrap().into(),
        definition_hash: "1".repeat(64),
        transient: false,
        version: live.version.clone(),
        instance_id: live.instance_id.clone(),
        ready: live.ready,
        connection_generation: live.connection_generation,
        quarantined: live.quarantined,
    };
    let preparing = service_update::begin(
        &controller,
        &barrier,
        &guard,
        &live,
        &identity,
        &config,
        "0.7.3",
    )
    .unwrap();
    let persisted = storage::read_private(&record_path).unwrap();
    assert!(!String::from_utf8_lossy(&persisted).contains(&token));
    assert_eq!(
        service_update::read(&prefix).unwrap(),
        Some(preparing.clone())
    );
    assert!(ControllerLock::acquire(&prefix, &binary).is_err());
    assert!(ManagerLock::for_service(&controller, &binary, false, false).is_err());
    drop(barrier);
    drop(guard);
    assert_eq!(
        unsafe { libc::kill(second.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), second.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(peer);
    let barrier = LaunchBarrier::acquire(&prefix).unwrap();
    let manager = ManagerLock::for_service(&controller, &binary, false, false).unwrap();
    let rollback =
        service_update::select_trial(&controller, &barrier, &manager, &preparing, true).unwrap();
    assert!(
        service_update::select_trial(&controller, &barrier, &manager, &preparing, false).is_err()
    );
    rdh_agent::native_install::restore_service(&manager, &rollback).unwrap();
    drop(manager);
    drop(barrier);

    let mut recovered = command(&binary, &temp.0)
        .args(launch_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut peer = tokio::time::timeout(Duration::from_secs(10), connected.recv())
        .await
        .unwrap()
        .unwrap();
    let guard = RunningInstall::inspect_at(&prefix, &binary).unwrap();
    let connecting = rdh_agent::readiness::probe_profile(&guard, &prefix, &config)
        .await
        .unwrap();
    assert!(
        !connecting.ready && connecting.quarantined,
        "{connecting:?}"
    );
    hub.ready.add_permits(1);
    // Receiving hello or sending ready on the Hub does not acknowledge that
    // the Agent consumed ready and bound the connection. Wait on its actual
    // readiness API, under the same existing bounded deadline as other launches.
    let ready = snapshot(&binary, &temp.0, &config).await;
    assert_eq!(ready["quarantined"], true);
    let trial = rdh_agent::readiness::probe_profile(&guard, &prefix, &config)
        .await
        .unwrap();
    assert!(trial.ready && trial.quarantined, "{trial:?}");
    assert_eq!(
        request(&mut peer, "rollback-trial", "rollback-trial").await["execution_state"],
        "not_sent"
    );
    let identity = UnitIdentity {
        pid: trial.pid,
        invocation_id: "a".repeat(32),
        version: trial.version.clone(),
        instance_id: trial.instance_id.clone(),
        ready: trial.ready,
        connection_generation: trial.connection_generation,
        quarantined: trial.quarantined,
        ..identity
    };
    let barrier = LaunchBarrier::acquire(&prefix).unwrap();
    assert!(
        service_update::accept(&controller, &barrier, &guard, &rollback, &trial, &identity)
            .is_err()
    );
    let committed =
        service_update::commit(&controller, &barrier, &guard, &rollback, &trial, &identity)
            .unwrap();
    let active = rdh_agent::readiness::activate_profile(&guard, &prefix, &config, &trial)
        .await
        .unwrap();
    assert!(!active.quarantined);
    let active_identity = UnitIdentity {
        quarantined: active.quarantined,
        ..identity
    };
    let accepted = service_update::accept(
        &controller,
        &barrier,
        &guard,
        &committed,
        &active,
        &active_identity,
    )
    .unwrap();
    service_update::finish(&controller, &barrier, &accepted).unwrap();
    assert!(!record_path.exists());
    assert!(
        request(&mut peer, "rollback-accepted", "rollback-accepted").await["result"]["sha256"]
            .is_string()
    );
    drop(barrier);
    drop(guard);
    assert_eq!(
        unsafe { libc::kill(recovered.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(10), recovered.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server.abort();
}
