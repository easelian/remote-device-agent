// Run the complete session only when every backend it exercises is enabled.
#![cfg(any(
    target_os = "linux",
    all(
        target_os = "macos",
        feature = "experimental-macos-files",
        feature = "experimental-macos-processes",
        feature = "experimental-macos-audit",
        feature = "experimental-macos-startup"
    )
))]

#[path = "support/initial_binary.rs"]
mod initial_binary;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path as FilePath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rdh_agent::config::sha256_hex;
use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};

struct Fixture {
    origin: String,
    root: String,
    token_hash: Mutex<Option<String>>,
    starts: AtomicUsize,
    result: Mutex<Option<oneshot::Sender<Vec<Value>>>>,
}

async fn start(State(state): State<Arc<Fixture>>, Json(value): Json<Value>) -> Response {
    if value["roots"] != json!({"work":state.root}) || value["token_hash"].as_str().is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    *state.token_hash.lock().await = value["token_hash"].as_str().map(str::to_owned);
    state.starts.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::CREATED,
        Json(json!({
            "verification_uri_complete": format!("{}/admin/pair?code=TEST-CODE", state.origin),
            "user_code":"TEST-CODE", "device_code":"fixture-code", "expires_in":600, "interval":3,
        })),
    )
        .into_response()
}

async fn poll(State(state): State<Arc<Fixture>>, Json(value): Json<Value>) -> Response {
    if value["device_code"] != "fixture-code" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Json(json!({"status":"approved", "config":{
        "gateway_url":state.origin, "device_id":"fixture-device", "roots":{"work":state.root},
        "allow_shell":true, "allow_write":true, "shell":"/bin/sh", "max_processes":4,
        "max_process_seconds":3600,
    }}))
    .into_response()
}

async fn request(
    socket: &mut WebSocket,
    id: &str,
    operation: &str,
    params: Value,
) -> Result<Value, String> {
    socket
        .send(Message::Text(
            json!({"type":"request", "id":id, "operation":operation, "params":params})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = socket.recv().await {
            let Message::Text(text) = message.map_err(|error| error.to_string())? else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
            if value["type"] == "result" && value["id"] == id {
                if value.get("error").is_some() {
                    return Err(format!("{operation}: {value}"));
                }
                return Ok(value);
            }
        }
        Err(format!("Connection closed while waiting for {operation}"))
    })
    .await
    .map_err(|_| format!("Timed out waiting for {operation}"))?
}

async fn completed_process(
    socket: &mut WebSocket,
    request_id: &str,
    job_id: &str,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = request(socket, request_id, "read_process", json!({"job_id":job_id})).await?;
        if result["result"]["status"] != "running" && result["result"]["cleanup_pending"] == false {
            return Ok(result);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("Process cleanup did not finish: {result}"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn process_roundtrip(socket: &mut WebSocket, results: &mut Vec<Value>) -> Result<(), String> {
    let started = request(
        socket,
        "process-start",
        "start_process",
        // Shell builtins keep the child's PATH empty too: there is no Node or
        // external helper available through the fixture's command search path.
        json!({"root":"work", "command":"IFS= read -r line; while IFS= read -r extra; do :; done; printf 'reply:%s\\n' \"$line\"; exit 7", "wait_seconds":0, "timeout_seconds":10}),
    )
    .await?;
    let id = started["result"]["job_id"]
        .as_str()
        .ok_or_else(|| format!("Missing process identity: {started}"))?
        .to_owned();
    results.push(started);
    results.push(request(socket, "process-list", "list_processes", json!({})).await?);
    results.push(
        request(
            socket,
            "process-input",
            "write_process_stdin",
            json!({"job_id":id,"text":"private-process-fixture😀\n","close":true}),
        )
        .await?,
    );
    results.push(completed_process(socket, "process-read", &id).await?);
    let blocked = request(
        socket,
        "blocked-start",
        "start_process",
        json!({"root":"work","command":"while IFS= read -r line; do :; done","wait_seconds":0,"timeout_seconds":10}),
    )
    .await?;
    let id = blocked["result"]["job_id"]
        .as_str()
        .ok_or_else(|| format!("Missing process identity: {blocked}"))?
        .to_owned();
    results.push(blocked);
    results.push(
        request(
            socket,
            "process-stop",
            "terminate_process",
            json!({"job_id":id}),
        )
        .await?,
    );
    results.push(completed_process(socket, "process-stopped", &id).await?);
    Ok(())
}

async fn agent(
    State(state): State<Arc<Fixture>>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let expected = state.token_hash.lock().await.clone();
    if device_id != "fixture-device" || token.map(|token| sha256_hex(token.as_bytes())) != expected
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    upgrade.on_upgrade(move |mut socket| async move {
        let Some(Ok(Message::Text(hello))) = socket.recv().await else { return; };
        let Ok(hello) = serde_json::from_str::<Value>(&hello) else { return; };
        if socket.send(Message::Text(json!({"type":"ready", "device_id":"fixture-device"}).to_string().into())).await.is_err() { return; }
        for message in [
            json!({"type":"request", "id":"info", "operation":"device_info", "params":{}, "execution_budget_ms":10000}),
            json!({"type":"request", "id":"invalid", "operation":"device_info", "params":{}, "execution_budget_ms":0}),
            json!({"type":"request", "id":"unknown", "operation":"unsupported", "params":{}}),
            json!({"type":"request", "id":"startup-info", "operation":"get_startup", "params":{}}),
            json!({"type":"request", "id":"startup-bypass", "operation":"set_startup", "params":{"mode":"user","approved":true}}),
            json!({"type":"request", "id":"startup-request", "operation":"set_startup", "params":{"mode":"user"}}),
        ] {
            if socket.send(Message::Text(message.to_string().into())).await.is_err() { return; }
        }
        let mut results = vec![hello];
        while results.len() < 7 {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) if value.get("type").and_then(Value::as_str) == Some("result") => results.push(value),
                    _ => {},
                },
                _ => return,
            }
        }
        let Some(instance) = results[0]["info"]["capabilities"]["operation_receipts"]["instance_id"].as_str().map(str::to_owned) else { return; };
        let operation_id = "a".repeat(32);
        let write = json!({"type":"request", "id":"write", "operation":"write_file",
            "params":{"root":"work","path":"entry.txt","content":"private-audit-fixture"},
            "operation_receipt":{"operation_id":operation_id, "instance_id":instance}});
        if socket.send(Message::Text(write.to_string().into())).await.is_err() { return; }
        let mut got_write = false;
        let mut got_receipt = false;
        while !got_write || !got_receipt {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) if value["type"] == "result" && value["id"] == "write" => { got_write = true; results.push(value); },
                    Ok(value) if value["type"] == "operation_result" && value["operation_id"] == operation_id => {
                        got_receipt = true;
                        results.push(value);
                        let ack = json!({"type":"operation_ack","operation_id":operation_id,"instance_id":instance});
                        if socket.send(Message::Text(ack.to_string().into())).await.is_err() { return; }
                    },
                    _ => {},
                },
                _ => return,
            }
        }
        for message in [
            json!({"type":"request","id":"read","operation":"read_file","params":{"root":"work","path":"entry.txt"}}),
            json!({"type":"request","id":"denied","operation":"write_file","params":{"root":"work","path":".env","content":"blocked"}}),
            json!({"type":"request","id":"status","operation":"get_operation_status","params":{"operation_id":operation_id,"instance_id":instance}}),
        ] {
            if socket.send(Message::Text(message.to_string().into())).await.is_err() { return; }
        }
        while results.len() < 12 {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) if value.get("type").and_then(Value::as_str) == Some("result") => results.push(value),
                    _ => {},
                },
                _ => return,
            }
        }
        if let Err(error) = process_roundtrip(&mut socket, &mut results).await {
            results.push(json!({"fixture_error":error}));
        }
        if let Some(sender) = state.result.lock().await.take() { let _ = sender.send(results); }
        while socket.recv().await.is_some() {}
    }).into_response()
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn diagnostics(path: &FilePath) -> String {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .take(65_537)
        .read_to_end(&mut bytes)
        .unwrap();
    let truncated = bytes.len() > 65_536;
    bytes.truncate(65_536);
    format!(
        "{}{}",
        String::from_utf8_lossy(&bytes),
        if truncated {
            "\n[fixture log truncated]"
        } else {
            ""
        }
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_binary_pairs_connects_reuses_identity_and_stops_cleanly_without_node() {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temporary(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-pair-session-{}-{}",
        std::process::id(),
        rdh_agent::random::hex_token::<8>().unwrap()
    )));
    std::fs::create_dir(&temp.0).unwrap();
    std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = temp.0.join("work");
    std::fs::create_dir(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (result_tx, result_rx) = oneshot::channel();
    let fixture = Arc::new(Fixture {
        origin: origin.clone(),
        root: root.to_str().unwrap().to_owned(),
        token_hash: Mutex::new(None),
        starts: AtomicUsize::new(0),
        result: Mutex::new(Some(result_tx)),
    });
    let router = Router::new()
        .route("/api/pair/start", post(start))
        .route("/api/pair/poll", post(poll))
        .route("/agent/{id}", get(agent))
        .with_state(Arc::clone(&fixture));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let config = temp.0.join("device.json");
    let policy = temp.0.join("device.policy.json");
    std::fs::write(
        &policy,
        serde_json::to_vec(
            &json!({"version":1,"protected_paths":[".env"],"audit":{"required":true}}),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600)).unwrap();
    let log_path = temp.0.join("agent.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = tokio::process::Command::new(initial_binary::path())
        .env_clear()
        .env("PATH", &temp.0)
        .env("HOME", &temp.0)
        .env("XDG_CONFIG_HOME", temp.0.join("xdg-config"))
        .env("XDG_STATE_HOME", temp.0.join("agent-state"))
        .args([
            "--server",
            &origin,
            "--config",
            config.to_str().unwrap(),
            "--root",
            &format!("work={}", root.display()),
            "--no-browser",
        ])
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let messages = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(15), result_rx) => match result {
            Ok(Ok(messages)) => Ok(messages),
            Ok(Err(error)) => Err(format!("Session fixture disconnected: {error}")),
            Err(_) => Err("Session fixture did not finish within 15 seconds".into()),
        },
        status = child.wait() => Err(format!("Agent exited before session completion: {status:?}")),
    };
    let messages = match messages {
        Ok(messages) => messages,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("{error}\n{}", diagnostics(&log_path));
        }
    };
    assert!(
        messages
            .iter()
            .all(|value| value.get("fixture_error").is_none()),
        "session results: {messages:?}"
    );
    assert_eq!(messages[0]["type"], "hello");
    assert_eq!(messages[0]["version"], 1);
    let operations = messages[0]["info"]["capabilities"]["operations"]
        .as_array()
        .unwrap();
    assert!(
        operations.contains(&json!("device_info")) && operations.contains(&json!("write_file"))
    );
    assert!(
        operations.contains(&json!("start_process")) && operations.contains(&json!("set_startup"))
    );
    assert_eq!(messages[0]["info"]["allow_shell"], true);
    assert_eq!(messages[0]["info"]["allow_write"], true);
    assert_eq!(
        messages[0]["info"]["hardening"]["local_audit"]["required"],
        true
    );
    let receipt = messages
        .iter()
        .find(|value| value["type"] == "operation_result")
        .unwrap();
    assert_eq!(receipt["execution_state"], "succeeded");
    assert_eq!(
        receipt["result_summary"]["sha256"],
        sha256_hex(b"private-audit-fixture")
    );
    let results: BTreeMap<_, _> = messages
        .into_iter()
        .skip(1)
        .filter(|result| result["type"] == "result")
        .map(|result| (result["id"].as_str().unwrap().to_owned(), result))
        .collect();
    assert!(results["info"]["result"]["hostname"].is_string());
    assert_eq!(results["invalid"]["error_code"], "operation_timeout");
    assert_eq!(results["invalid"]["execution_state"], "not_sent");
    assert_eq!(results["unknown"]["error_code"], "agent_error");
    assert_eq!(results["startup-info"]["result"]["mode"], "unknown");
    assert_eq!(results["startup-bypass"]["error_code"], "invalid_params");
    assert_eq!(
        results["startup-request"]["result"]["status"], "pending_local_approval",
        "startup response: {}",
        results["startup-request"]
    );
    assert_eq!(
        results["write"]["result"]["sha256"],
        sha256_hex(b"private-audit-fixture")
    );
    assert_eq!(
        results["read"]["result"]["content"],
        "private-audit-fixture"
    );
    assert_eq!(results["denied"]["error_code"], "policy_path_protected");
    assert_eq!(results["denied"]["execution_state"], "not_sent");
    assert_eq!(results["status"]["result"]["execution_state"], "succeeded");
    let job = &results["process-start"]["result"]["job_id"];
    assert!(job.is_string());
    assert!(
        results["process-list"]["result"]["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|process| &process["job_id"] == job && process["status"] == "running")
    );
    assert_eq!(results["process-input"]["result"]["stdin_closed"], true);
    assert_eq!(results["process-read"]["result"]["job_id"], *job);
    assert_eq!(results["process-read"]["result"]["status"], "exited");
    assert_eq!(results["process-read"]["result"]["exit_code"], 7);
    assert_eq!(results["process-read"]["result"]["cleanup_pending"], false);
    assert_eq!(results["process-read"]["result"]["error"], Value::Null);
    assert_eq!(
        results["process-read"]["result"]["output"],
        "reply:private-process-fixture😀\n"
    );
    assert_eq!(
        results["process-stopped"]["result"]["job_id"],
        results["blocked-start"]["result"]["job_id"]
    );
    assert_eq!(results["process-stopped"]["result"]["status"], "terminated");
    assert_eq!(
        results["process-stopped"]["result"]["cleanup_pending"],
        false
    );
    assert_eq!(results["process-stopped"]["result"]["error"], Value::Null);
    assert!(!root.join(".env").exists());
    let metadata = std::fs::metadata(&config).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o077, 0);
    let saved: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(saved["device_id"], "fixture-device");
    assert_eq!(
        Some(sha256_hex(saved["token"].as_str().unwrap().as_bytes())),
        *fixture.token_hash.lock().await
    );
    let request_id = results["startup-request"]["result"]["request_id"]
        .as_str()
        .unwrap();
    let approval = tokio::process::Command::new(initial_binary::path())
        .env_clear()
        .env("PATH", &temp.0)
        .env("HOME", &temp.0)
        .env("XDG_CONFIG_HOME", temp.0.join("xdg-config"))
        .args([
            "startup",
            "approve",
            request_id,
            "--config",
            config.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(!approval.status.success());
    assert!(String::from_utf8_lossy(&approval.stderr).contains("interactive terminal"));
    let denial = tokio::process::Command::new(initial_binary::path())
        .env_clear()
        .env("PATH", &temp.0)
        .env("HOME", &temp.0)
        .env("XDG_CONFIG_HOME", temp.0.join("xdg-config"))
        .args([
            "startup",
            "deny",
            request_id,
            "--config",
            config.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        denial.status.success(),
        "{}",
        String::from_utf8_lossy(&denial.stderr)
    );
    let state: Value =
        serde_json::from_slice(&std::fs::read(temp.0.join("device.json.startup.json")).unwrap())
            .unwrap();
    assert_eq!(state["pending"], Value::Null);
    let pid = child.id().unwrap();
    // SAFETY: this PID is the live child created by this test; SIGTERM requests graceful shutdown.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{}", diagnostics(&log_path));
    let audit = std::fs::read_to_string(
        temp.0
            .join("agent-state/remote-device-hub/audit/fixture-device.jsonl"),
    )
    .unwrap();
    assert!(!audit.contains("private-audit-fixture"));
    assert!(!audit.contains("private-process-fixture"));
    assert!(audit.lines().any(|line| line.contains("\"id\":\"write\"") && line.contains("\"op\":\"write_file\"")));
    assert!(
        audit.lines().any(
            |line| line.contains("\"op\":\"process_exit\"") && line.contains("\"exit_code\":7")
        )
    );
    assert!(!temp.0.join("device.json.rdh-lock").exists());
    let output = tokio::process::Command::new(initial_binary::path())
        .env_clear()
        .env("PATH", &temp.0)
        .env("HOME", &temp.0)
        .args([
            "--server",
            &origin,
            "--config",
            config.to_str().unwrap(),
            "--pair-only",
            "--no-browser",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Using saved pairing: fixture-device")
    );
    assert_eq!(fixture.starts.load(Ordering::SeqCst), 1);
    server.abort();
}
