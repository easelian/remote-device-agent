//! Real Chromium + production RPC/receipt path; explicitly opt in with a browser binary.
#![cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(
        windows,
        feature = "experimental-windows-private-storage",
        feature = "experimental-windows-processes"
    )
))]
use axum::{
    Json, Router,
    extract::State,
    response::Html,
    routing::{get, post},
};
use rdh_agent::{config::DeviceConfig, policy::Policy, random, rpc::Runtime, transport};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>RDH browser fixture</title>
<h1>Test customer</h1><form id="form"><label>Name <input id="name" name="name"></label><button>Save customer</button></form><p id="result"></p>
<label>Secret <input type="password" value="MUST_NOT_DISCLOSE"></label><a href="/next">Next page</a>
<script>form.onsubmit=async e=>{e.preventDefault();const r=await fetch('/save',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({name:document.querySelector('#name').value})});document.querySelector('#result').textContent='Saved '+(await r.json()).name;};</script>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn abrupt_runtime_exit_reclaims_owned_browser_without_replaying_input() {
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    if let Some(root) = std::env::var_os("RDH_BROWSER_CRASH_CHILD_ROOT") {
        let root = PathBuf::from(root);
        let url = std::env::var("RDH_BROWSER_CRASH_CHILD_URL").unwrap();
        let mut peer = profile_peer(&root, &executable, false);
        let opened = peer
            .call(
                "browser_open",
                json!({"url":url,"profile_name":"Crash fixture"}),
            )
            .await;
        assert!(opened.get("error").is_none(), "{opened}");
        let id = opened["result"]["session_id"].as_str().unwrap();
        let observed = peer.observe_named(id, 1, "Record once").await;
        let acted = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Record once")}})).await;
        assert!(acted.get("error").is_none(), "{acted}");
        fs::write(
            root.join("opened.json"),
            serde_json::to_vec(&opened["result"]).unwrap(),
        )
        .unwrap();
        std::future::pending::<()>().await;
        unreachable!();
    }
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-crash-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let saved = received.clone();
    let app = Router::new()
        .route("/", get(|| async { Html("<!doctype html><button onclick=\"fetch('/record',{method:'POST'})\">Record once</button>") }))
        .route("/record", post(move || { let saved = saved.clone(); async move { saved.fetch_add(1, Ordering::AcqRel); "recorded" } }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for missing_owner in if cfg!(windows) {
        vec![false, true]
    } else {
        vec![false]
    } {
        let root = temp.0.join(if missing_owner {
            "before-owner-publication"
        } else {
            "active-session"
        });
        rdh_agent::storage::create_private_dir(&root).unwrap();
        let log = fs::File::create(root.join("child.log")).unwrap();
        let expected = received.load(Ordering::Acquire) + 1;
        let mut child = Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "abrupt_runtime_exit_reclaims_owned_browser_without_replaying_input",
                    "--ignored",
                    "--nocapture",
                ])
                .env("RDH_BROWSER_CRASH_CHILD_ROOT", &root)
                .env("RDH_BROWSER_CRASH_CHILD_URL", &url)
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let until = std::time::Instant::now() + Duration::from_secs(30);
        let opened = loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(root.join("child.log")).unwrap()
            );
            if let Ok(bytes) = fs::read(root.join("opened.json"))
                && let Ok(opened) = serde_json::from_slice::<Value>(&bytes)
                && received.load(Ordering::Acquire) == expected
            {
                break opened;
            }
            assert!(
                std::time::Instant::now() < until,
                "{}",
                fs::read_to_string(root.join("child.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let id = opened["session_id"].as_str().unwrap();
        let run = fs::read_dir(root.join(".rdh-browser"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().join(id))
            .find(|p| p.is_dir())
            .unwrap();
        if missing_owner {
            // Simulate death after creation but before common owner publication.
            // Only the independent empty-Job witness can retire this directory.
            fs::remove_file(run.join("owner.json")).unwrap();
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let mut peer = profile_peer(&root, &executable, false);
        let mut reopened = Value::Null;
        for _ in 0..30 {
            reopened = peer
                .call(
                    "browser_open",
                    json!({"url":url,"profile_name":"Crash fixture"}),
                )
                .await;
            if reopened.get("error").is_none() {
                break;
            }
            assert!(
                matches!(
                    reopened["error_code"].as_str(),
                    Some("browser_cleanup_pending" | "browser_profile_busy")
                ),
                "{reopened}"
            );
            assert_eq!(reopened["execution_state"], "not_sent", "{reopened}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(reopened.get("error").is_none(), "{reopened}");
        assert_eq!(reopened["result"]["profile_id"], opened["profile_id"]);
        assert!(!run.exists(), "old browser run directory was not retired");
        let id = reopened["result"]["session_id"].as_str().unwrap();
        peer.observe_named(id, 1, "Record once").await;
        assert_eq!(
            received.load(Ordering::Acquire),
            expected,
            "restart replayed website input"
        );
        peer.call("browser_control", json!({"session_id":id,"action":"close"}))
            .await;
        assert!(peer.runtime.shutdown().await);
        drop(peer);
    }
    server.abort();
}

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Preserved failing browser fixture: {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Peer {
    runtime: Arc<Runtime>,
    sender: transport::Sender,
    receiver: tokio::sync::mpsc::Receiver<transport::Frame>,
    ids: Arc<Mutex<HashSet<String>>>,
    counter: u64,
    budget: u64,
}
impl Peer {
    async fn raw(
        &mut self,
        operation: &str,
        params: Value,
        owner: Option<&str>,
        receipt: Option<&str>,
    ) -> Value {
        self.raw_as(
            operation,
            params,
            owner.map(|owner| json!({"version":1,"owner":owner,"user":owner,"channel":"ai"})),
            receipt,
        )
        .await
    }
    async fn raw_as(
        &mut self,
        operation: &str,
        params: Value,
        principal: Option<Value>,
        receipt: Option<&str>,
    ) -> Value {
        self.counter += 1;
        let id = format!("browser-test-{}", self.counter);
        let mut message = json!({"id":id,"operation":operation,"params":params,"execution_budget_ms":self.budget});
        if let Some(principal) = principal {
            message["browser_principal"] = principal;
        }
        if let Some(receipt) = receipt {
            message["operation_receipt"] =
                json!({"operation_id":receipt,"instance_id":self.runtime.receipts.instance_id});
        }
        self.runtime
            .receive(&message, &self.sender, &self.ids)
            .unwrap();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(20), self.receiver.recv())
                .await
                .expect("RPC response deadline")
                .unwrap();
            let value: Value = serde_json::from_str(frame.message.to_text().unwrap()).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }
    async fn human(&mut self, params: Value) -> Value {
        self.raw_as(
            "browser_human",
            params,
            Some(
                json!({"version":1,"owner":"c".repeat(64),"user":"a".repeat(64),"channel":"human"}),
            ),
            None,
        )
        .await
    }
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        let receipt = rdh_agent::browser::MUTATIONS
            .contains(&operation)
            .then(|| random::hex_token::<16>().unwrap());
        self.raw(operation, params, Some(&"a".repeat(64)), receipt.as_deref())
            .await
    }
    async fn observe(&mut self, id: &str, epoch: u64) -> Value {
        self.observe_named(id, epoch, "Save customer").await
    }
    async fn observe_named(&mut self, id: &str, epoch: u64, name: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..40 {
            let reply = self
                .call(
                    "browser_observe",
                    json!({"session_id":id,"control_epoch":epoch}),
                )
                .await;
            if reply["result"]["nodes"].as_array().is_some_and(|v| {
                v.iter()
                    .any(|n| n["name"].as_str().is_some_and(|s| s.trim() == name.trim()))
            }) {
                return reply["result"].clone();
            }
            assert!(
                !matches!(
                    reply["error_code"].as_str(),
                    Some("browser_lost" | "browser_io_error")
                ),
                "{reply}"
            );
            assert!(
                reply.get("error").is_none()
                    || matches!(
                        reply["error_code"].as_str(),
                        Some("browser_input_pending" | "browser_stale_reference")
                    ),
                "{reply}"
            );
            last = reply;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("fixture not observable: {last}");
    }
}
fn reference(observation: &Value, name: &str) -> String {
    let nodes: Vec<_> = observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| {
            n["name"].as_str().is_some_and(|n| n.trim() == name.trim()) && n.get("ref").is_some()
        })
        .collect();
    assert_eq!(nodes.len(), 1, "{observation}");
    nodes[0]["ref"].as_str().unwrap().into()
}

fn names_of(observation: &Value) -> Vec<String> {
    observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_owned())
        .collect()
}
fn node_named<'a>(observation: &'a Value, name: &str) -> &'a Value {
    let nodes: Vec<_> = observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["name"] == name)
        .collect();
    assert_eq!(nodes.len(), 1, "{observation}");
    nodes[0]
}

fn profile_peer(root: &std::path::Path, executable: &str, downloads: bool) -> Peer {
    browser_peer(root, executable, downloads, false)
}
fn browser_peer(root: &std::path::Path, executable: &str, downloads: bool, uploads: bool) -> Peer {
    policy_peer(
        root,
        json!({"enabled":true,"persistent_profiles":true,"downloads":downloads,"uploads":uploads,"executable":executable}),
    )
}
/// A peer whose local policy holds the given `browser` settings.
fn policy_peer(root: &std::path::Path, browser: Value) -> Peer {
    device_peer(root, json!({"version":1,"browser":browser}), false).unwrap()
}
/// A peer for a device with the given local policy, with or without a shell.
/// Fails where the device could not start with that policy.
fn device_peer(root: &std::path::Path, policy: Value, shell: bool) -> Result<Peer, String> {
    let config = DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8123","device_id":"profile-test","token":"x".repeat(48),"roots":rdh_agent::config::local_roots(&[format!("work={}", root.display())]).unwrap(),"allow_write":false,"allow_shell":shell})).unwrap();
    let policy = Policy::parse(&policy, &config).map_err(|error| error.to_string())?;
    let runtime =
        Runtime::with_config_path(config, policy, None, None, Some(&root.join("device.json")))
            .map_err(|error| error.to_string())?;
    let (sender, receiver, _) = transport::channel();
    Ok(Peer {
        runtime,
        sender,
        receiver,
        ids: Arc::new(Mutex::new(HashSet::new())),
        counter: 0,
        budget: 15000,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn bounded_download_delivery_privacy_cancel_and_recovery() {
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-file-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let content: Vec<u8> = (0..=255u8).cycle().take(240000).collect();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let payload = content.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let payload = payload.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                while request.len() < 8192 && !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let path = std::str::from_utf8(&request)
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                if path == "/" {
                    let body=b"<!doctype html><title>File fixture</title><h1>File task</h1><a href='/report'>Report</a><a href='/short'>Truncated</a><a href='/large'>Too large</a><a href='/unknown'>Unknown length</a><a href='/stall'>Slow report</a><a href='/reserve' target='_blank'>Budget hold</a>";
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    return;
                }
                let length = if path == "/large" {
                    33554433
                } else if path == "/reserve" {
                    33554432
                } else {
                    payload.len()
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=report.bin\r\nCache-Control: no-store\r\n{}\r\n",
                    if path == "/unknown" {
                        String::new()
                    } else {
                        format!("Content-Length: {length}\r\n")
                    }
                );
                let _ = socket.write_all(header.as_bytes()).await;
                if path == "/stall" || path == "/reserve" {
                    let _ = socket.write_all(&payload[..4096]).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                } else {
                    let data = if path == "/short" {
                        &payload[..97]
                    } else if path == "/large" {
                        &[]
                    } else {
                        payload.as_slice()
                    };
                    let _ = socket.write_all(data).await;
                }
            });
        }
    });
    let mut peer = profile_peer(&temp.0, &executable, true);
    let opened = peer.call("browser_open", json!({"url":url})).await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap();
    let observation = peer.observe_named(id, 1, "Report").await;
    let clicked=peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observation,"Report")}})).await;
    assert!(clicked.get("error").is_none(), "{clicked}");
    let mut ready = Value::Null;
    for _ in 0..100 {
        let files = peer.call("browser_files", json!({})).await;
        let file = &files["result"]["files"][0];
        if file["state"] == "ready" {
            ready = file.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ready["size_bytes"], content.len());
    let artifact = ready["artifact_id"].as_str().unwrap().to_owned();
    assert_eq!(ready["sha256"], rdh_agent::config::sha256_hex(&content));
    assert_eq!(
        peer.raw("browser_files", json!({}), Some(&"b".repeat(64)), None)
            .await["result"]["files"],
        json!([])
    );
    let denied = peer
        .raw_as(
            "browser_human",
            json!({"action":"file_read","artifact_id":artifact,"offset":0}),
            Some(
                json!({"version":1,"owner":"c".repeat(64),"user":"b".repeat(64),"channel":"human"}),
            ),
            None,
        )
        .await;
    assert_eq!(denied["error_code"], "browser_file_unavailable");
    let claim = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"files-view"}))
        .await;
    assert!(claim.get("error").is_none(), "{claim}");
    assert_eq!(
        peer.call("browser_files", json!({})).await["result"]["files"],
        json!([])
    );
    let mut received = Vec::new();
    loop {
        let chunk = peer
            .human(json!({"action":"file_read","artifact_id":artifact,"offset":received.len()}))
            .await;
        assert!(chunk.get("error").is_none(), "{chunk}");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk["result"]["data"].as_str().unwrap())
            .unwrap();
        assert!(bytes.len() <= 65536);
        received.extend(bytes);
        if chunk["result"]["eof"] == true {
            break;
        }
    }
    assert_eq!(received, content);
    let released=peer.human(json!({"action":"release","session_id":id,"view_id":"files-view","lease_id":claim["result"]["lease"]["lease_id"],"control_epoch":claim["result"]["lease"]["control_epoch"],"share_page_with_ai":true})).await;
    assert!(released.get("error").is_none(), "{released}");
    let epoch = released["result"]["control_epoch"].as_u64().unwrap();
    for (name, code) in [
        ("Truncated", "browser_file_incomplete"),
        ("Too large", "browser_file_limit"),
        ("Unknown length", "browser_download_unsupported"),
    ] {
        let observation = peer.observe_named(id, epoch, name).await;
        let clicked=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"click","ref":reference(&observation,name)}})).await;
        assert!(clicked.get("error").is_none(), "{clicked}");
        let mut failed = false;
        for _ in 0..100 {
            let files = peer.call("browser_files", json!({})).await;
            if files["result"]["recent_failures"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["error_code"] == code)
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(failed, "missing rejection for {name}");
        assert_eq!(
            peer.call("browser_files", json!({})).await["result"]["files"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    let observation = peer.observe_named(id, epoch, "Slow report").await;
    let clicked=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"click","ref":reference(&observation,"Slow report")}})).await;
    assert!(clicked.get("error").is_none(), "{clicked}");
    let mut slow = String::new();
    for _ in 0..100 {
        let files = peer.call("browser_files", json!({})).await;
        if let Some(file) = files["result"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["state"] == "receiving")
        {
            slow = file["artifact_id"].as_str().unwrap().into();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!slow.is_empty());
    let cancelled = peer
        .human(json!({"action":"file_delete","artifact_id":slow}))
        .await;
    assert_eq!(cancelled["result"]["cancel_requested"], true, "{cancelled}");
    let mut stopped = false;
    for _ in 0..100 {
        let files = peer.call("browser_files", json!({})).await;
        if files["result"]["files"].as_array().unwrap().len() == 1 {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(stopped);
    // Reserve declared sizes before receiving bytes. Three 32-MiB streams plus
    // the completed report fit; a fourth must not create another staged file.
    let old_failures = peer.call("browser_files", json!({})).await["result"]["recent_failures"]
        .as_array()
        .unwrap()
        .len();
    for expected in 2..=4 {
        let observation = peer.observe_named(id, epoch, "Budget hold").await;
        let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"click","ref":reference(&observation,"Budget hold")}})).await;
        assert!(clicked.get("error").is_none(), "{clicked}");
        let mut count = 0;
        for _ in 0..250 {
            count = peer.call("browser_files", json!({})).await["result"]["files"]
                .as_array()
                .unwrap()
                .len();
            if count == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            count,
            expected,
            "{}",
            peer.call("browser_files", json!({})).await
        );
    }
    let observation = peer.observe_named(id, epoch, "Budget hold").await;
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"click","ref":reference(&observation,"Budget hold")}})).await;
    assert!(clicked.get("error").is_none(), "{clicked}");
    let mut last = Value::Null;
    for _ in 0..100 {
        last = peer.call("browser_files", json!({})).await["result"].clone();
        if last["recent_failures"].as_array().unwrap().len() > old_failures {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(last["files"].as_array().unwrap().len(), 4, "{last}");
    assert_eq!(
        last["recent_failures"].as_array().unwrap().last().unwrap()["error_code"],
        "browser_file_limit"
    );
    for file in last["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["state"] == "receiving")
    {
        let cancel = peer
            .human(json!({"action":"file_delete","artifact_id":file["artifact_id"]}))
            .await;
        assert_eq!(cancel["result"]["cancel_requested"], true, "{cancel}");
    }
    assert_eq!(
        peer.call("browser_control", json!({"session_id":id,"action":"close"}))
            .await["result"]["state"],
        "closed"
    );
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    let mut peer = profile_peer(&temp.0, &executable, true);
    let files = peer.call("browser_files", json!({})).await;
    assert_eq!(files["result"]["files"][0]["artifact_id"], artifact);
    let deleted = peer
        .call(
            "browser_control",
            json!({"action":"delete_file","artifact_id":artifact}),
        )
        .await;
    assert_eq!(deleted["result"]["file_deleted"], true, "{deleted}");
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn named_profile_persistence_isolation_and_exclusive_ownership() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").expect("set exact test browser path");
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-profile-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/", get(||async {Html(r#"<!doctype html><meta charset="utf-8"><title>Profile fixture</title><h1 id="state"></h1><button id="remember">Remember</button><script>function show(){const stored=localStorage.getItem('fixture')==='saved',cookie=document.cookie.includes('fixture=saved');document.querySelector('#state').textContent=stored&&cookie?'Stored login':stored?'Missing cookie':cookie?'Missing storage':'Fresh profile';}remember.onclick=()=>{localStorage.setItem('fixture','saved');document.cookie='fixture=saved; Max-Age=86400; SameSite=Lax';show();};show();</script>"#)}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call(
            "browser_open",
            json!({"url":url,"profile_name":"工作账号 中文"}),
        )
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap();
    let profile_id = opened["result"]["profile_id"].clone();
    let observation = peer.observe_named(id, 1, "Remember").await;
    assert!(
        observation["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Fresh profile")
    );
    let action = peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observation,"Remember")}})).await;
    assert!(action.get("error").is_none(), "{action}");
    peer.observe_named(id, 1, "Stored login").await;
    // Reservation is checked before a second browser can open or delete this data.
    let duplicate = peer
        .call(
            "browser_open",
            json!({"url":url,"profile_name":"工作账号 中文"}),
        )
        .await;
    assert_eq!(
        duplicate["error_code"], "browser_profile_busy",
        "{duplicate}"
    );
    let deleted = peer
        .call(
            "browser_control",
            json!({"action":"delete_profile","profile_name":"工作账号 中文"}),
        )
        .await;
    assert_eq!(deleted["error_code"], "browser_profile_busy", "{deleted}");
    let listed = peer.call("browser_profiles", json!({})).await;
    assert_eq!(
        listed["result"]["profiles"],
        json!([{"profile_id":profile_id,"name":"工作账号 中文","in_use":true,"retention":"until_deleted"}])
    );
    let other = peer
        .raw("browser_profiles", json!({}), Some(&"b".repeat(64)), None)
        .await;
    assert_eq!(other["result"]["profiles"], json!([]));
    // A second Runtime with the same device binding must not run recovery against live data.
    let mut competing = profile_peer(&temp.0, &executable, false);
    let blocked = competing
        .call("browser_open", json!({"profile_name":"different"}))
        .await;
    assert_eq!(blocked["error_code"], "browser_profile_busy", "{blocked}");
    assert!(competing.runtime.shutdown().await);
    drop(competing);
    assert_eq!(
        peer.call("browser_control", json!({"session_id":id,"action":"close"}))
            .await["result"]["state"],
        "closed"
    );
    // Normal close persists both local storage and a persistent cookie.
    let reopened = peer
        .call(
            "browser_open",
            json!({"url":url,"profile_name":"工作账号 中文"}),
        )
        .await;
    assert!(reopened.get("error").is_none(), "{reopened}");
    assert_eq!(reopened["result"]["profile_id"], profile_id);
    let id = reopened["result"]["session_id"].as_str().unwrap();
    peer.observe_named(id, 1, "Stored login").await;
    let human = peer
        .human(json!({"action":"status","session_id":id,"view_id":"profile-view"}))
        .await;
    assert_eq!(human["result"]["profile_name"], "工作账号 中文");
    assert_eq!(
        peer.call("browser_control", json!({"session_id":id,"action":"close"}))
            .await["result"]["state"],
        "closed"
    );
    // Another principal with the same display name gets separate on-disk data.
    let other = peer
        .raw(
            "browser_open",
            json!({"url":url,"profile_name":"工作账号 中文"}),
            Some(&"b".repeat(64)),
            Some(&random::hex_token::<16>().unwrap()),
        )
        .await;
    assert!(other.get("error").is_none(), "{other}");
    assert_ne!(other["result"]["profile_id"], profile_id);
    let other_id = other["result"]["session_id"].as_str().unwrap();
    let observed = peer
        .raw(
            "browser_wait",
            json!({"session_id":other_id,"control_epoch":1,"text_contains":"Fresh profile"}),
            Some(&"b".repeat(64)),
            None,
        )
        .await;
    assert_eq!(observed["result"]["condition_observed"], true, "{observed}");
    assert_eq!(
        peer.raw(
            "browser_control",
            json!({"session_id":other_id,"action":"close"}),
            Some(&"b".repeat(64)),
            Some(&random::hex_token::<16>().unwrap())
        )
        .await["result"]["state"],
        "closed"
    );
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    // Recreating the Agent runtime neither adopts nor replays any old session.
    let mut peer = profile_peer(&temp.0, &executable, false);
    assert_eq!(
        peer.call("browser_sessions", json!({})).await["result"]["sessions"],
        json!([])
    );
    let reopened = peer
        .call(
            "browser_open",
            json!({"url":url,"profile_name":"工作账号 中文"}),
        )
        .await;
    assert!(reopened.get("error").is_none(), "{reopened}");
    let id = reopened["result"]["session_id"].as_str().unwrap();
    peer.observe_named(id, 1, "Stored login").await;
    assert_eq!(
        peer.call("browser_control", json!({"session_id":id,"action":"close"}))
            .await["result"]["state"],
        "closed"
    );
    let deleted = peer
        .call(
            "browser_control",
            json!({"action":"delete_profile","profile_name":"工作账号 中文"}),
        )
        .await;
    assert_eq!(deleted["result"]["profile_deleted"], true, "{deleted}");
    assert_eq!(
        peer.call("browser_profiles", json!({})).await["result"]["profiles"],
        json!([])
    );
    let fresh = peer
        .call(
            "browser_open",
            json!({"url":url,"profile_name":"工作账号 中文"}),
        )
        .await;
    assert!(fresh.get("error").is_none(), "{fresh}");
    let id = fresh["result"]["session_id"].as_str().unwrap();
    peer.observe_named(id, 1, "Fresh profile").await;
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn browser_rpc_task_ownership_recovery_and_cleanup() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").expect("set exact test browser path");
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let saves = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/select",get(||async {Html(r#"<!doctype html><meta charset="utf-8"><style>body{min-height:3000px}</style><label>Region<select id="region"><option value="">Choose</option><option value="cn-n">华北</option><option value="off" disabled>Disabled</option></select></label><p id="status"></p><script>let inputSeen=false;region.oninput=()=>inputSeen=true;region.onchange=async()=>{await fetch('/save',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({selection:region.value,inputSeen})});document.querySelector('#status').textContent='Selected 华北';};window.onscroll=()=>fetch('/save',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({scrollObserved:scrollY})});</script>"#)}))
        .route("/human", get(||async {Html(r#"<!doctype html><meta charset="utf-8"><h1>Human login</h1><form id="login"><input id="secret" type="password" autofocus aria-label="Password"><button>Sign in</button></form><script>let replaced=false;setInterval(async()=>{if(!replaced&&(await(await fetch('/focus-state')).json())){replaced=true;const copy=secret.cloneNode(true);secret.replaceWith(copy);copy.focus();await fetch('/save',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({focusReplaced:true})});}},25);login.onsubmit=async e=>{e.preventDefault();await fetch('/save',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({humanSecret:secret.value})});document.body.innerHTML='<h1>Logged in</h1>';};</script>"#)}))
        .route("/focus-state", get(|State(saves): State<Arc<Mutex<Vec<Value>>>>| async move { Json(saves.lock().unwrap().iter().any(|v|v["replaceFocused"]==true)) }))
        .route("/dialog",get(||async {Html("<h1>Dialog task</h1><button onclick=\"alert('Synthetic dialog')\">Open dialog</button>")}))
        .route("/next", get(|| async { Html("<h1>Next document</h1>") }))
        .route("/large", get(|| async { Html(format!("<h1>Large page</h1>{}", "<p>bounded text</p>".repeat(50000))) }))
        .route("/overlay", get(|headers: axum::http::HeaderMap| async move {
            let host = headers["host"].to_str().unwrap().replace("127.0.0.1", "localhost");
            Html(format!("<h1>Covered frame</h1><iframe width=500 height=260 src='http://{host}/frame/covered'></iframe><div style='position:fixed;inset:0;z-index:999;background:#ffffff66'>Overlay</div>"))
        }))
        .route(
            "/save",
            post(
                |State(saves): State<Arc<Mutex<Vec<Value>>>>, Json(value): Json<Value>| async move {
                    saves.lock().unwrap().push(value.clone());
                    Json(value)
                },
            ),
        )
        .route("/frames", get(|headers: axum::http::HeaderMap| async move {
            let host = headers["host"].to_str().unwrap().replace("127.0.0.1", "localhost");
            Html(format!("<h1>Frame task</h1><iframe width=500 height=260 src='/frame/local'></iframe><iframe width=500 height=260 src='http://{host}/frame/cross'></iframe>"))
        }))
        .route("/frame/{kind}", get(|axum::extract::Path(kind): axum::extract::Path<String>| async move {
            Html(PAGE.replace("Name ", &format!("{kind} name ")).replace("Save customer", &format!("Save {kind} customer")))
        }))
        .with_state(saves.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config=DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8123","device_id":"browser-test","token":"x".repeat(48),"roots":rdh_agent::config::local_roots(&[format!("work={}", temp.0.display())]).unwrap(),"allow_write":false,"allow_shell":false})).unwrap();
    let policy = Policy::parse(
        &json!({"version":1,"browser":{"enabled":true,"executable":executable}}),
        &config,
    )
    .unwrap();
    let runtime = Runtime::with_config_path(
        config,
        policy,
        None,
        None,
        Some(&temp.0.join("device.json")),
    )
    .unwrap();
    assert_eq!(runtime.info()["capabilities"]["browser"]["available"], true);
    assert_eq!(
        runtime.info()["capabilities"]["browser"]["profiles"],
        json!(["temporary"])
    );
    let (sender, receiver, _) = transport::channel();
    let mut peer = Peer {
        runtime: runtime.clone(),
        sender,
        receiver,
        ids: Arc::new(Mutex::new(HashSet::new())),
        counter: 0,
        budget: 15000,
    };
    let denied = peer.raw("browser_open", json!({}), None, None).await;
    assert_eq!(denied["error_code"], "browser_unauthorized");
    let denied = peer
        .call("browser_open", json!({"profile_name":"not-enabled"}))
        .await;
    assert_eq!(denied["error_code"], "browser_profile_disabled");
    let denied = peer
        .call("browser_open", json!({"url":"file:///etc/passwd"}))
        .await;
    assert_eq!(denied["error_code"], "browser_url_denied");
    let oid = random::hex_token::<16>().unwrap();
    let open = peer
        .raw(
            "browser_open",
            json!({"url":url}),
            Some(&"a".repeat(64)),
            Some(&oid),
        )
        .await;
    assert!(open.get("error").is_none(), "{open}");
    let id = open["result"]["session_id"].as_str().unwrap().to_owned();
    let repeated = peer
        .raw(
            "browser_open",
            json!({"url":url}),
            Some(&"a".repeat(64)),
            Some(&oid),
        )
        .await;
    assert_eq!(repeated["execution_state"], "succeeded");
    assert_eq!(repeated["result_summary"]["session_id"], id);
    let receipt = peer
        .call(
            "get_operation_status",
            json!({"operation_id":oid,"instance_id":runtime.receipts.instance_id}),
        )
        .await;
    assert_eq!(receipt["result"]["result_summary"]["session_id"], id);
    assert!(!receipt.to_string().contains(&url));
    let other = peer
        .raw("browser_sessions", json!({}), Some(&"b".repeat(64)), None)
        .await;
    assert_eq!(other["result"]["sessions"], json!([]));
    let denied = peer
        .raw(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1}),
            Some(&"b".repeat(64)),
            None,
        )
        .await;
    assert_eq!(denied["error_code"], "browser_session_unavailable");
    let observation = peer.observe(&id, 1).await;
    assert_eq!(observation["page"]["url"], url);
    assert_eq!(observation["page"]["title"], "RDH browser fixture");
    assert!(!observation.to_string().contains("MUST_NOT_DISCLOSE"));
    let screenshot = peer
        .call(
            "browser_screenshot",
            json!({"session_id":id,"control_epoch":1}),
        )
        .await;
    assert!(screenshot.get("error").is_none(), "{screenshot}");
    assert_eq!(screenshot["result"]["mime_type"], "image/jpeg");
    assert!(screenshot["result"]["data"].as_str().unwrap().len() < 512 * 1024);
    assert!(screenshot["result"]["width"].as_u64().unwrap() <= 1920);
    let old_ref = reference(&observation, "Name ");
    let fill=peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","ref":old_ref,"text":"客户 😀"}})).await;
    assert!(fill.get("error").is_none(), "{fill}");
    let stale=peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","ref":old_ref,"text":"wrong"}})).await;
    assert_eq!(stale["error_code"], "browser_stale_reference");
    let observation = peer.observe(&id, 1).await;
    assert!(
        observation["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["value"] == "客户 😀")
    );
    let save = reference(&observation, "Save customer");
    let args = json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":save}});
    let save_id = random::hex_token::<16>().unwrap();
    let saved = peer
        .raw(
            "browser_act",
            args.clone(),
            Some(&"a".repeat(64)),
            Some(&save_id),
        )
        .await;
    assert!(saved.get("error").is_none(), "{saved}");
    let duplicate = peer
        .raw("browser_act", args, Some(&"a".repeat(64)), Some(&save_id))
        .await;
    assert_eq!(duplicate["execution_state"], "succeeded");
    for _ in 0..30 {
        if !saves.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(*saves.lock().unwrap(), vec![json!({"name":"客户 😀"})]);
    let waited = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Saved 客户 😀"}),
        )
        .await;
    assert_eq!(waited["result"]["condition_observed"], true, "{waited}");
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{url}frames")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let observation = peer.observe_named(&id, 1, "Save cross customer").await;
    assert_eq!(
        observation["coverage"]["frames_observed"], 3,
        "{observation}"
    );
    for kind in ["local", "cross"] {
        let observation = peer
            .observe_named(&id, 1, &format!("Save {kind} customer"))
            .await;
        let name = reference(&observation, &format!("{kind} name "));
        let fill=peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","ref":name,"text":kind}})).await;
        assert!(fill.get("error").is_none(), "{fill}");
        let observation = peer
            .observe_named(&id, 1, &format!("Save {kind} customer"))
            .await;
        let save = reference(&observation, &format!("Save {kind} customer"));
        let click = peer
            .call(
                "browser_act",
                json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":save}}),
            )
            .await;
        assert!(click.get("error").is_none(), "{click}");
        for _ in 0..30 {
            if saves.lock().unwrap().iter().any(|v| v["name"] == kind) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saves.lock().unwrap().iter().any(|v| v["name"] == kind),
            "frame {kind}: {:?}",
            *saves.lock().unwrap()
        );
    }
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{url}large")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let large = peer.observe_named(&id, 1, "Large page").await;
    assert_eq!(large["coverage"]["complete_within_scope"], false, "{large}");
    assert!(
        large["coverage"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "observation_budget")
    );
    assert!(large["nodes"].as_array().unwrap().len() <= 200);
    assert!(large["coverage"]["nodes_scanned"].as_u64().unwrap() <= 5000);
    assert!(large.to_string().len() < 96000);
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{url}overlay")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let covered = peer.observe_named(&id, 1, "Save covered customer").await;
    let click = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&covered,"Save covered customer")}})).await;
    assert_eq!(click["error_code"], "browser_not_actionable", "{click}");
    assert_eq!(
        saves.lock().unwrap().len(),
        3,
        "an obscured frame must not submit"
    );
    let paused = peer
        .call("browser_control", json!({"session_id":id,"action":"pause"}))
        .await;
    assert!(paused.get("error").is_none(), "{paused}");
    let epoch = paused["result"]["control_epoch"].as_u64().unwrap();
    let hidden = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":epoch}),
        )
        .await;
    assert_eq!(hidden["error_code"], "browser_paused");
    let resumed = peer
        .call(
            "browser_control",
            json!({"session_id":id,"control_epoch":epoch,"action":"resume"}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let old_epoch = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1}),
        )
        .await;
    assert_eq!(old_epoch["error_code"], "browser_control_changed");
    let epoch = resumed["result"]["control_epoch"].as_u64().unwrap();
    let dialog_page=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":format!("{url}dialog")}})).await;
    assert!(dialog_page.get("error").is_none(), "{dialog_page}");
    let observed = peer.observe_named(&id, epoch, "Open dialog").await;
    let dialog_click=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"click","ref":reference(&observed,"Open dialog")}})).await;
    assert_eq!(
        dialog_click["error_code"], "browser_dialog_open",
        "dialog input: {dialog_click}"
    );
    let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
    assert!(tabs["result"]["tabs"][0]["dialog"].is_object(), "{tabs}");
    let epoch = tabs["result"]["control_epoch"].as_u64().unwrap();
    let dialog=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"dialog","accept":false,"dialog_id":tabs["result"]["tabs"][0]["dialog"]["dialog_id"]}})).await;
    assert!(dialog.get("error").is_none(), "dialog handling: {dialog}");
    // The click that opened the dialog is answered only after the dialog closes,
    // and some browser versions answer it a moment later. Until then a control
    // change reports pending input, so wait for it to settle as a client would.
    let mut resumed = Value::Null;
    let mut waits = 0;
    for _ in 0..100 {
        resumed = peer
            .call(
                "browser_control",
                json!({"session_id":id,"control_epoch":epoch,"action":"resume"}),
            )
            .await;
        if resumed["error_code"] != "browser_input_pending" {
            break;
        }
        waits += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        resumed.get("error").is_none(),
        "after dialog ({waits} waits): {resumed}"
    );
    let epoch = resumed["result"]["control_epoch"].as_u64().unwrap();
    let navigate=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":format!("{url}human")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    peer.observe_named(&id, epoch, "Human login").await;
    let human_tab =
        peer.call("browser_tabs", json!({"session_id":id})).await["result"]["active_tab"].clone();
    let second = peer
        .call(
            "browser_act",
            json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"new_tab"}}),
        )
        .await;
    assert!(second.get("error").is_none(), "{second}");
    let invalid_human = peer
        .call(
            "browser_human",
            json!({"action":"claim","session_id":id,"view_id":"test-view"}),
        )
        .await;
    assert_eq!(invalid_human["error_code"], "browser_unauthorized");
    let claimed = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"test-view"}))
        .await;
    assert!(claimed.get("error").is_none(), "{claimed}");
    let lease = claimed["result"]["lease"].clone();
    let epoch = lease["control_epoch"].as_u64().unwrap();
    let hidden = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":epoch}),
        )
        .await;
    assert_eq!(hidden["error_code"], "browser_paused");
    let resume = peer
        .call(
            "browser_control",
            json!({"session_id":id,"control_epoch":epoch,"action":"resume"}),
        )
        .await;
    assert_eq!(resume["error_code"], "browser_paused");
    let tabs_picture=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch})).await;
    assert_eq!(
        tabs_picture["result"]["tabs"].as_array().unwrap().len(),
        2,
        "{tabs_picture}"
    );
    let switched=peer.human(json!({"action":"input","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"sequence":1,"frame_id":tabs_picture["result"]["frame_id"],"input":{"kind":"tab","tab_id":human_tab}})).await;
    assert!(switched.get("error").is_none(), "{switched}");
    let old_picture=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch})).await;
    assert_eq!(
        old_picture["result"]["text_input_available"], true,
        "{old_picture}"
    );
    saves.lock().unwrap().push(json!({"replaceFocused":true}));
    for _ in 0..40 {
        if saves
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["focusReplaced"] == true)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        saves
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["focusReplaced"] == true)
    );
    let replaced=peer.human(json!({"action":"input","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"sequence":2,"frame_id":old_picture["result"]["frame_id"],"input":{"kind":"text","text":"MUST-NOT-INSERT"}})).await;
    assert_eq!(replaced["error_code"], "browser_stale_frame", "{replaced}");
    for (sequence, input) in [
        (3, json!({"kind":"text","text":"SYNTHETIC-human-秘密"})),
        (4, json!({"kind":"key","key":"Enter"})),
    ] {
        let picture=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch})).await;
        assert!(picture.get("error").is_none(), "{picture}");
        assert_eq!(picture["result"]["mime_type"], "image/jpeg");
        assert!(
            picture["result"]["data"]
                .as_str()
                .unwrap()
                .starts_with("/9j/")
        );
        let args = json!({"action":"input","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"sequence":sequence,"frame_id":picture["result"]["frame_id"],"input":input});
        let sent = peer.human(args.clone()).await;
        assert!(sent.get("error").is_none(), "{sent}");
        let repeated = peer.human(args).await;
        assert_eq!(
            repeated["error_code"], "browser_duplicate_input",
            "{repeated}"
        );
    }
    for _ in 0..40 {
        if saves
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["humanSecret"] == "SYNTHETIC-human-秘密")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        saves
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["humanSecret"] == "SYNTHETIC-human-秘密")
            .count(),
        1
    );
    let unconfirmed=peer.human(json!({"action":"release","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"share_page_with_ai":false})).await;
    assert_eq!(unconfirmed["error_code"], "browser_invalid_params");
    let released=peer.human(json!({"action":"release","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"share_page_with_ai":true})).await;
    assert!(released.get("error").is_none(), "{released}");
    let observed = peer
        .observe_named(
            &id,
            released["result"]["control_epoch"].as_u64().unwrap(),
            "Logged in",
        )
        .await;
    assert!(!observed.to_string().contains("SYNTHETIC-human"));
    let epoch = released["result"]["control_epoch"].as_u64().unwrap();
    let navigate = peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":format!("{url}select")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let observed = peer.observe_named(&id, epoch, "Region").await;
    let region = observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "Region" && node.get("ref").is_some())
        .unwrap();
    assert_eq!(region["options"][1]["value"], "cn-n");
    assert_eq!(region["options"][1]["label"], "华北");
    assert_eq!(region["options"][2]["disabled"], true);
    let selected = peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"select","ref":region["ref"],"value":"cn-n"}})).await;
    assert!(selected.get("error").is_none(), "{selected}");
    let waited = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":epoch,"text_contains":"Selected 华北"}),
        )
        .await;
    assert!(waited.get("error").is_none(), "{waited}");
    assert!(
        saves
            .lock()
            .unwrap()
            .iter()
            .any(|v| *v == json!({"selection":"cn-n","inputSeen":true}))
    );
    let scrolled = peer
        .call(
            "browser_act",
            json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"scroll","delta_y":600}}),
        )
        .await;
    assert!(scrolled.get("error").is_none(), "{scrolled}");
    assert!(scrolled["result"]["scroll_y"].as_f64().unwrap() >= 600.);
    for _ in 0..40 {
        if saves
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["scrollObserved"].as_f64().is_some_and(|y| y >= 600.))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saves
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["scrollObserved"].as_f64().is_some_and(|y| y >= 600.))
    );
    let close = peer
        .call("browser_control", json!({"session_id":id,"action":"close"}))
        .await;
    assert_eq!(close["result"]["state"], "closed", "{close}");
    assert!(runtime.shutdown().await);
    drop(peer);
    drop(runtime);
    let private = temp.0.join(".rdh-browser");
    for owner in fs::read_dir(private).unwrap() {
        let remaining = fs::read_dir(owner.unwrap().path())
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].file_name(), "artifacts");
        assert_eq!(fs::read_dir(remaining[0].path()).unwrap().count(), 0);
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn human_approved_upload_is_private_bound_and_single_use() {
    use base64::Engine;
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-upload-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    type Uploads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;
    let received: Uploads = Arc::new(Mutex::new(Vec::new()));
    let app=Router::new().route("/",get(||async{Html(r#"<!doctype html><meta charset="utf-8"><title>Upload fixture</title><label>Upload proof <input id="file" type="file"></label><p id="state">Ready</p><script>file.onchange=async()=>{const f=file.files[0];await fetch('/upload',{method:'POST',headers:{'x-file-name':f.name},body:f});document.querySelector('#state').textContent='Upload received';};</script>"#)}))
        .route("/upload",post(|State(received):State<Uploads>,headers:axum::http::HeaderMap,body:axum::body::Bytes|async move {
            received.lock().unwrap().push((headers["x-file-name"].to_str().unwrap().into(),body.to_vec()));"ok"
        })).with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let url = format!("{origin}/");
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = browser_peer(&temp.0, &executable, false, true);
    let opened = peer.call("browser_open", json!({"url":url})).await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap();
    peer.observe_named(id, 1, "Upload proof").await;
    let claim = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"upload-view"}))
        .await;
    assert!(claim.get("error").is_none(), "{claim}");
    let lease = &claim["result"]["lease"];
    let base = json!({"session_id":id,"view_id":"upload-view","lease_id":lease["lease_id"],"control_epoch":lease["control_epoch"]});
    let tabs = peer
        .human({
            let mut p = base.clone();
            p["action"] = "screenshot".into();
            p
        })
        .await;
    let tab = tabs["result"]["page"]["tab_id"].clone();
    let bytes: Vec<u8> = (0..=255).cycle().take(150001).collect();
    let sha = rdh_agent::config::sha256_hex(&bytes);
    let mut begin = base.clone();
    begin.as_object_mut().unwrap().extend(json!({"action":"upload_begin","tab_id":tab,"source_origin":"https://wrong.test","name":"upload-proof.bin","size_bytes":bytes.len(),"sha256":sha,"approved":true}).as_object().unwrap().clone());
    assert_eq!(
        peer.human(begin.clone()).await["error_code"],
        "browser_upload_denied"
    );
    begin["source_origin"] = origin.into();
    let started = peer.human(begin).await;
    assert!(started.get("error").is_none(), "{started}");
    let grant = started["result"]["file_grant_id"].clone();
    assert_eq!(started["result"]["offset"], 0);
    assert_eq!(
        peer.call("browser_files", json!({})).await["result"]["uploads"],
        json!([])
    );
    for (index, chunk) in bytes.chunks(65536).enumerate() {
        let mut params = base.clone();
        params.as_object_mut().unwrap().extend(json!({"action":"upload_chunk","file_grant_id":grant,"offset":index*65536,"data":base64::engine::general_purpose::STANDARD.encode(chunk)}).as_object().unwrap().clone());
        let reply = peer.human(params.clone()).await;
        assert_eq!(
            reply["result"]["next_offset"],
            index * 65536 + chunk.len(),
            "{reply}"
        );
        assert_eq!(
            peer.human(params).await["error_code"],
            "browser_upload_offset"
        );
    }
    let mut finish = base.clone();
    finish["action"] = "upload_finish".into();
    finish["file_grant_id"] = grant.clone();
    assert_eq!(peer.human(finish).await["result"]["state"], "approved");
    let mut release = base;
    release["action"] = "release".into();
    release["share_page_with_ai"] = true.into();
    let released = peer.human(release).await;
    let epoch = released["result"]["control_epoch"].as_u64().unwrap();
    let observation = peer.observe_named(id, epoch, "Upload proof").await;
    let file_ref = reference(&observation, "Upload proof");
    assert!(
        observation["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["ref"] == file_ref && n["file_input"] == true)
    );
    let listing = peer.call("browser_files", json!({})).await;
    assert_eq!(listing["result"]["uploads"][0]["state"], "approved");
    assert!(
        !serde_json::to_string(&listing)
            .unwrap()
            .contains("payload/")
    );
    peer.call(
        "browser_act",
        json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":url}}),
    )
    .await;
    peer.call(
        "browser_wait",
        json!({"session_id":id,"control_epoch":epoch,"text_contains":"Upload proof"}),
    )
    .await;
    let stale=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"attach_file","ref":file_ref,"file_grant_id":grant}})).await;
    assert_eq!(stale["error_code"], "browser_stale_reference");
    assert_eq!(
        peer.call("browser_files", json!({})).await["result"]["uploads"][0]["state"],
        "approved"
    );
    let observation = peer.observe_named(id, epoch, "Upload proof").await;
    let params = json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"attach_file","ref":reference(&observation,"Upload proof"),"file_grant_id":grant}});
    let receipt = random::hex_token::<16>().unwrap();
    let selected = peer
        .raw(
            "browser_act",
            params.clone(),
            Some(&"a".repeat(64)),
            Some(&receipt),
        )
        .await;
    assert_eq!(selected["result"]["file_selected"], true, "{selected}");
    peer.raw("browser_act", params, Some(&"a".repeat(64)), Some(&receipt))
        .await;
    peer.observe_named(id, epoch, "Upload received").await;
    assert_eq!(
        *received.lock().unwrap(),
        vec![("upload-proof.bin".into(), bytes)]
    );
    let observation = peer.observe_named(id, epoch, "Upload proof").await;
    let reused=peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"attach_file","ref":reference(&observation,"Upload proof"),"file_grant_id":grant}})).await;
    assert_eq!(reused["error_code"], "browser_upload_denied");
    assert_eq!(received.lock().unwrap().len(), 1);
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn bounded_page_continuation_preserves_text_and_reports_changed_documents() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-page-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let rows = (0..6000)
        .map(|i| format!("<p>Row {i:04}</p>"))
        .collect::<String>();
    let text = "甲😀e\u{301}段落".repeat(500);
    let text_page = format!(
        "<!doctype html><meta charset=utf-8><p>{text}</p><script>fetch('/ready-text',{{method:'POST'}})</script>"
    );
    let loaded = Arc::new(AtomicUsize::new(0));
    let scan_loaded = loaded.clone();
    let text_loaded = loaded.clone();
    let mutation = Arc::new(AtomicUsize::new(0));
    let app=Router::new()
        .route("/long",get(move||{let page=format!("<!doctype html><meta charset=utf-8>{rows}");async{Html(page)}}))
        .route("/scan",get(||async{Html(format!("<!doctype html>{}<p>Tail reached</p><script>fetch('/ready-scan',{{method:'POST'}})</script>","<span></span>".repeat(12000)))}))
        .route("/text",get(move||{let page=text_page.clone();async{Html(page)}}))
        .route("/ready-scan",post(move||{let loaded=scan_loaded.clone();async move {loaded.store(1,Ordering::Release);"ok"}}))
        .route("/ready-text",post(move||{let loaded=text_loaded.clone();async move {loaded.store(2,Ordering::Release);"ok"}}))
        .route("/changing",get(||async{Html("<!doctype html><p>Before change</p><p>Second paragraph</p><script>let done=false;setInterval(async()=>{if(!done&&(await(await fetch('/change')).json())){done=true;document.querySelector('p').textContent='After change';await fetch('/seen',{method:'POST'});}},20);</script>")}))
        .route("/shadow-changing",get(||async{Html("<!doctype html><div id='host'></div><script>const root=document.querySelector('#host').attachShadow({mode:'open'});root.innerHTML='<p>'+('Shadow before '.repeat(100))+'</p>';let done=false;setInterval(async()=>{if(!done&&(await(await fetch('/change')).json())){done=true;root.querySelector('p').firstChild.nodeValue='Shadow after '.repeat(100);await fetch('/seen',{method:'POST'});}},20);</script>")}))
        .route("/moving",get(||async{Html("<!doctype html><p id='first'>Moved row</p><p>Middle row</p><p>End row</p><script>let done=false;setInterval(async()=>{if(!done&&(await(await fetch('/change')).json())){done=true;document.body.appendChild(document.getElementById('first'));await fetch('/seen',{method:'POST'});}},20);</script>")}))
        .route("/change",get(|State(state):State<Arc<AtomicUsize>>|async move{Json(state.load(Ordering::Acquire)>0)}))
        .route("/seen",post(|State(state):State<Arc<AtomicUsize>>|async move{state.store(2,Ordering::Release);"ok"})).with_state(mutation.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/long")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap();
    peer.observe_named(id, 1, "Row 0000").await;
    let mut cursor = Value::Null;
    let mut names = Vec::new();
    let mut pages = 0;
    loop {
        let mut params =
            json!({"session_id":id,"control_epoch":1,"max_nodes":1000,"max_chars":64000});
        if !cursor.is_null() {
            params["cursor"] = cursor.clone();
        }
        let reply = peer.call("browser_observe", params).await;
        assert!(reply.get("error").is_none(), "{reply}");
        let value = &reply["result"];
        assert!(value["coverage"]["nodes_scanned"].as_u64().unwrap() <= 5000);
        let nodes = value["nodes"].as_array().unwrap();
        assert!(nodes.len() <= 1000);
        names.extend(nodes.iter().map(|n| n["name"].as_str().unwrap().to_owned()));
        pages += 1;
        cursor = value["next_cursor"].clone();
        if cursor.is_null() {
            assert_eq!(value["coverage"]["sequence_complete"], true);
            break;
        }
        assert!(pages < 15);
    }
    assert_eq!(
        names,
        (0..6000).map(|i| format!("Row {i:04}")).collect::<Vec<_>>()
    );
    assert!(pages >= 6);
    for path in ["scan", "text"] {
        peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/{path}")}})).await;
        // Wait for the fixture's independent end-of-document signal. A new URL
        // can be visible before any DOM text is parsed on a native browser.
        let expected_loaded = if path == "scan" { 1 } else { 2 };
        for _ in 0..80 {
            if loaded.load(Ordering::Acquire) == expected_loaded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(loaded.load(Ordering::Acquire), expected_loaded);
        let mut first = Value::Null;
        for _ in 0..40 {
            let candidate = peer
                .call(
                    "browser_observe",
                    json!({"session_id":id,"control_epoch":1,"max_nodes":4,"max_chars":128}),
                )
                .await;
            if candidate.get("error").is_none()
                && candidate["result"]["page"]["url"] == format!("{origin}/{path}")
            {
                first = candidate;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!first.is_null());
        let mut reply = first;
        let mut assembled = String::new();
        let mut pages = 0;
        loop {
            assert!(reply.get("error").is_none(), "{reply}");
            let value = &reply["result"];
            for node in value["nodes"].as_array().unwrap() {
                assembled.push_str(node["name"].as_str().unwrap());
            }
            pages += 1;
            let Some(next) = value["next_cursor"].as_str() else {
                assert_eq!(value["coverage"]["sequence_complete"], true);
                break;
            };
            assert!(pages < 80);
            reply=peer.call("browser_observe",json!({"session_id":id,"control_epoch":1,"max_nodes":4,"max_chars":128,"cursor":next})).await;
        }
        if path == "scan" {
            assert_eq!(assembled, "Tail reached");
            assert!(pages >= 3);
        } else {
            assert_eq!(assembled, text);
        }
    }
    peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/changing")}})).await;
    peer.observe_named(id, 1, "Before change").await;
    let first = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"max_nodes":1}),
        )
        .await;
    let token = first["result"]["next_cursor"].clone();
    assert!(token.is_string());
    mutation.store(1, Ordering::Release);
    for _ in 0..100 {
        if mutation.load(Ordering::Acquire) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mutation.load(Ordering::Acquire), 2);
    // The page changed what the first segment had read. The sequence goes on
    // from where it stopped and says so, instead of being refused.
    let changed = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"cursor":token}),
        )
        .await;
    assert!(changed.get("error").is_none(), "{changed}");
    let rest = &changed["result"];
    assert_eq!(names_of(rest), ["Second paragraph"], "{changed}");
    assert_eq!(rest["coverage"]["changed_during_read"], true, "{changed}");
    assert_eq!(rest["coverage"]["sequence_complete"], false, "{changed}");
    let first = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"max_nodes":1}),
        )
        .await;
    let token = first["result"]["next_cursor"].clone();
    let claim = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"pagination"}))
        .await;
    let lease = &claim["result"]["lease"];
    let release=peer.human(json!({"action":"release","session_id":id,"view_id":"pagination","lease_id":lease["lease_id"],"control_epoch":lease["control_epoch"],"share_page_with_ai":true})).await;
    let stale=peer.call("browser_observe",json!({"session_id":id,"control_epoch":release["result"]["control_epoch"],"cursor":token})).await;
    assert_eq!(stale["error_code"], "browser_stale_cursor");
    mutation.store(0, Ordering::Release);
    peer.call("browser_act",json!({"session_id":id,"control_epoch":release["result"]["control_epoch"],"action":{"kind":"navigate","url":format!("{origin}/shadow-changing")}})).await;
    let epoch = release["result"]["control_epoch"].as_u64().unwrap();
    let mut first = Value::Null;
    for _ in 0..40 {
        let reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":epoch,"max_chars":128}),
            )
            .await;
        if reply["result"]["nodes"].as_array().is_some_and(|nodes| {
            nodes.iter().any(|n| {
                n["name"]
                    .as_str()
                    .is_some_and(|v| v.contains("Shadow before"))
            })
        }) {
            first = reply;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let token = first["result"]["next_cursor"].clone();
    assert!(token.is_string(), "{first}");
    mutation.store(1, Ordering::Release);
    for _ in 0..100 {
        if mutation.load(Ordering::Acquire) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mutation.load(Ordering::Acquire), 2);
    // A long text is read as it was when the reader came to it, so the rest of
    // it still belongs to the part already returned.
    let changed = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":epoch,"cursor":token}),
        )
        .await;
    assert!(changed.get("error").is_none(), "{changed}");
    let rest = &changed["result"];
    assert_eq!(
        names_of(&first["result"]).concat() + &names_of(rest).concat(),
        "Shadow before ".repeat(100),
        "{changed}"
    );
    assert_eq!(rest["coverage"]["changed_during_read"], true, "{changed}");
    assert_eq!(rest["coverage"]["sequence_complete"], false, "{changed}");
    // The row a sequence stopped on is moved behind the rows it has not read. It
    // goes on with the row that followed, and reads the moved one only once.
    mutation.store(0, Ordering::Release);
    peer.call("browser_act",json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":format!("{origin}/moving")}})).await;
    peer.observe_named(id, epoch, "End row").await;
    let first = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":epoch,"max_nodes":1}),
        )
        .await;
    assert_eq!(names_of(&first["result"]), ["Moved row"], "{first}");
    let token = first["result"]["next_cursor"].clone();
    mutation.store(1, Ordering::Release);
    for _ in 0..100 {
        if mutation.load(Ordering::Acquire) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mutation.load(Ordering::Acquire), 2);
    let moved = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":epoch,"cursor":token}),
        )
        .await;
    assert!(moved.get("error").is_none(), "{moved}");
    assert_eq!(
        names_of(&moved["result"]),
        ["Middle row", "End row"],
        "{moved}"
    );
    assert_eq!(
        moved["result"]["coverage"]["changed_during_read"], true,
        "{moved}"
    );
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

const REJECTIONS: &str = r#"<!doctype html><meta charset="utf-8"><title>Rejection fixture</title>
<h1>Rejections</h1><label>Name <input id="name" value="old"></label><label>Secret <input type="password"></label>
<button type="button">Plain button</button>
<label>Color <select><option value="red">Red</option><option value="green">Green</option></select></label>
<label><input type="checkbox" checked> Agree</label>
<input readonly value="2026-01-01" aria-label="Pick date" onclick="document.getElementById('picked').textContent='Date picker opened'"><p id="picked"></p>"#;

// A fixed banner lies over the whole page until its own button removes it.
const BANNER: &str = r#"<!doctype html><meta charset="utf-8"><title>Banner</title>
<button type="button" onclick="document.getElementById('out').textContent='Record saved'">Save record</button><p id="out"></p>
<div id="banner" style="position:fixed;inset:0;background:#ffffffcc"><button type="button" onclick="document.getElementById('banner').remove()">Accept cookies</button></div>"#;

// One button is scrolled out of an inner container, the other lies below the viewport.
const BELOW: &str = r#"<!doctype html><meta charset="utf-8"><title>Below</title>
<div style="height:120px;overflow:auto"><div style="height:600px"></div><button type="button" onclick="document.getElementById('out').textContent+=' inner'">Inner button</button></div>
<div style="height:3000px"></div><button type="button" onclick="document.getElementById('out').textContent+=' far'">Far button</button><p id="out">Clicked:</p>"#;

// The link's bounding box spans both lines, so its center lies on plain text.
const WRAPPED: &str = r#"<!doctype html><meta charset="utf-8"><title>Wrapped link</title>
<div style="width:400px;font:16px/60px monospace">Lead text before the <a href="/landed" aria-label="Wrapped link">Wrapped<br>link</a> and trailing words</div>"#;

/// The session is still under AI control in its first epoch.
async fn assert_running(peer: &mut Peer, id: &str) {
    let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
    assert_eq!(tabs["result"]["controller"], "ai", "{tabs}");
    assert_eq!(tabs["result"]["control_epoch"], 1, "{tabs}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn rejections_without_input_keep_the_session_running() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-rejection-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new()
        .route("/form", get(|| async { Html(REJECTIONS) }))
        .route("/wrapped", get(|| async { Html(WRAPPED) }))
        .route("/banner", get(|| async { Html(BANNER) }))
        .route("/reloading", get(|| async { Html("<!doctype html><title>Reloading</title><h1>Reloading</h1><script>setTimeout(()=>location.reload(),20)</script>") }))
        .route("/below", get(|| async { Html(BELOW) }))
        .route("/framed", get(|| async { Html(r#"<!doctype html><button type="button" onclick="this.textContent='Framed clicked'">Framed button</button>"#) }))
        .route("/below-frame", get(|headers: axum::http::HeaderMap| async move {
            // A different host puts the frame in its own process.
            let host = headers["host"].to_str().unwrap().replace("127.0.0.1", "localhost");
            Html(format!("<!doctype html><div style='height:3000px'></div><iframe width=400 height=200 src='http://{host}/framed'></iframe>"))
        }))
        .route(
            "/landed",
            get(|| async { Html("<!doctype html><h1>Landed</h1>") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/form")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    // The session names the browser build it runs on.
    let browser = &opened["result"]["browser"];
    assert!(
        browser["product"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "{opened}"
    );
    let major = browser["version"]
        .as_str()
        .and_then(|v| v.split('.').next()?.parse::<u32>().ok());
    assert!(major.is_some_and(|major| major >= 100), "{opened}");

    // An action that does not suit its element is rejected before any page effect.
    for (name, mut action, code) in [
        (
            "Secret",
            json!({"kind":"fill","text":"x"}),
            "browser_sensitive_input",
        ),
        (
            "Plain button",
            json!({"kind":"fill","text":"x"}),
            "browser_invalid_params",
        ),
        (
            "Color",
            json!({"kind":"select","value":"unobserved"}),
            "browser_invalid_params",
        ),
        (
            "Plain button",
            json!({"kind":"check","checked":true}),
            "browser_invalid_params",
        ),
    ] {
        let observed = peer.observe_named(&id, 1, "Plain button").await;
        action["ref"] = reference(&observed, name).into();
        let reply = peer
            .call(
                "browser_act",
                json!({"session_id":id,"control_epoch":1,"action":action}),
            )
            .await;
        assert_eq!(reply["error_code"], code, "{name}: {reply}");
        assert_eq!(reply["execution_state"], "not_sent", "{name}: {reply}");
        assert_running(&mut peer, &id).await;
    }
    // Later actions run in the same epoch without a resume.
    let observed = peer.observe_named(&id, 1, "Plain button").await;
    let unchanged = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"check","checked":true,"ref":reference(&observed,"Agree")}})).await;
    assert_eq!(unchanged["result"]["changed"], false, "{unchanged}");
    let observed = peer.observe_named(&id, 1, "Plain button").await;
    let filled = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","text":"new","ref":reference(&observed,"Name")}})).await;
    assert_eq!(filled["result"]["input_acknowledged"], true, "{filled}");
    let observed = peer.observe_named(&id, 1, "Plain button").await;
    let field = observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n.get("ref").is_some() && n["name"].as_str().is_some_and(|s| s.trim() == "Name"))
        .unwrap();
    assert_eq!(
        field["value"], "new",
        "fill replaces the earlier value: {observed}"
    );
    assert_running(&mut peer, &id).await;

    // A read-only field takes a click, but not text.
    let observed = peer.observe_named(&id, 1, "Plain button").await;
    let date = observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Pick date")
        .unwrap();
    assert_eq!(date["enabled"], true, "{observed}");
    assert_eq!(date["editable"], false, "{observed}");
    let refused = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","text":"x","ref":reference(&observed,"Pick date")}})).await;
    assert_eq!(refused["error_code"], "browser_not_actionable", "{refused}");
    assert_eq!(refused["execution_state"], "not_sent", "{refused}");
    let observed = peer.observe_named(&id, 1, "Plain button").await;
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Pick date")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let picked = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Date picker opened"}),
        )
        .await;
    assert_eq!(picked["result"]["condition_observed"], true, "{picked}");

    // A link wrapped over two lines is hit where the click lands: on its first
    // fragment, not at the center of a box that also covers other text.
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/wrapped")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let observed = peer.observe_named(&id, 1, "Wrapped link").await;
    let followed = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Wrapped link")}})).await;
    assert_eq!(followed["result"]["input_acknowledged"], true, "{followed}");
    let landed = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"url_contains":"/landed"}),
        )
        .await;
    assert_eq!(landed["result"]["condition_observed"], true, "{landed}");
    assert_running(&mut peer, &id).await;

    // A covered target in the top document is refused before any page effect.
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/banner")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let observed = peer.observe_named(&id, 1, "Save record").await;
    let covered = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Save record")}})).await;
    assert_eq!(covered["error_code"], "browser_not_actionable", "{covered}");
    assert_eq!(covered["execution_state"], "not_sent", "{covered}");
    assert_running(&mut peer, &id).await;
    let observed = peer.observe_named(&id, 1, "Save record").await;
    let accepted = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Accept cookies")}})).await;
    assert_eq!(accepted["result"]["input_acknowledged"], true, "{accepted}");
    let observed = peer.observe_named(&id, 1, "Save record").await;
    let saved = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Save record")}})).await;
    assert_eq!(saved["result"]["input_acknowledged"], true, "{saved}");
    let shown = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Record saved"}),
        )
        .await;
    assert_eq!(shown["result"]["condition_observed"], true, "{shown}");

    // Targets on a scrolled page: inside a container, below the viewport, and
    // in a frame below the viewport.
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/below")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    for (button, text) in [("Inner button", "inner"), ("Far button", "far")] {
        let observed = peer.observe_named(&id, 1, "Far button").await;
        let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,button)}})).await;
        assert_eq!(
            clicked["result"]["input_acknowledged"], true,
            "{button}: {clicked}"
        );
        let shown = peer
            .call(
                "browser_wait",
                json!({"session_id":id,"control_epoch":1,"text_contains":text}),
            )
            .await;
        assert_eq!(
            shown["result"]["condition_observed"], true,
            "{button}: {shown}"
        );
    }
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/below-frame")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let observed = peer.observe_named(&id, 1, "Framed button").await;
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Framed button")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let shown = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Framed clicked"}),
        )
        .await;
    assert_eq!(shown["result"]["condition_observed"], true, "{shown}");
    assert_running(&mut peer, &id).await;

    // Reading a page that keeps reloading can be refused, but it sends no input,
    // so it never reports an uncertain outcome and never pauses the session.
    let navigate = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/reloading")}})).await;
    assert!(navigate.get("error").is_none(), "{navigate}");
    let mut outcomes = std::collections::BTreeMap::<String, u32>::new();
    for _ in 0..60 {
        let reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":1}),
            )
            .await;
        assert_ne!(reply["execution_state"], "unknown", "{reply}");
        *outcomes
            .entry(reply["error_code"].as_str().unwrap_or("ok").to_owned())
            .or_default() += 1;
    }
    assert!(outcomes.get("ok").is_some_and(|n| *n > 0), "{outcomes:?}");
    let waited = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Reloading"}),
        )
        .await;
    assert_ne!(waited["execution_state"], "unknown", "{waited}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

const PAGE_STATE: &str = r##"<!doctype html><meta charset="utf-8"><title>Page state</title>
<label>Quantity <input type="number" value="3"></label>
<label>Volume <input type="range" min="0" max="10" value="7"></label>
<a href="#details">Show details</a><h2 id="details">Details</h2>"##;

fn value_of(observation: &Value, name: &str) -> Value {
    observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n.get("ref").is_some() && n["name"].as_str().is_some_and(|s| s.trim() == name))
        .unwrap_or_else(|| panic!("{name} is not in {observation}"))["value"]
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn values_and_addresses_are_reported_as_the_page_holds_them() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-state-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/state", get(|| async { Html(PAGE_STATE) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/state")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();

    // A number field and a slider report the number they hold.
    let observed = peer.observe_named(&id, 1, "Show details").await;
    assert_eq!(value_of(&observed, "Quantity"), "3", "{observed}");
    assert_eq!(value_of(&observed, "Volume"), "7", "{observed}");
    let filled = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","text":"42","ref":reference(&observed,"Quantity")}})).await;
    assert_eq!(filled["result"]["input_acknowledged"], true, "{filled}");
    let observed = peer.observe_named(&id, 1, "Show details").await;
    let moved = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"press","key":"ArrowRight","ref":reference(&observed,"Volume")}})).await;
    assert_eq!(moved["result"]["input_acknowledged"], true, "{moved}");
    let observed = peer.observe_named(&id, 1, "Show details").await;
    assert_eq!(value_of(&observed, "Quantity"), "42", "{observed}");
    assert_eq!(value_of(&observed, "Volume"), "8", "{observed}");

    // A link within the page changes only the fragment of the address. A wait sees it.
    let jumped = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Show details")}})).await;
    assert_eq!(jumped["result"]["input_acknowledged"], true, "{jumped}");
    let waited = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"url_contains":"#details"}),
        )
        .await;
    assert_eq!(waited["result"]["condition_observed"], true, "{waited}");
    assert_eq!(
        waited["result"]["url"],
        format!("{origin}/state#details"),
        "{waited}"
    );
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

// A clock ticks beside one button; another button sits in a list row whose
// text the page replaces on request.
const LIVE: &str = r#"<!doctype html><meta charset="utf-8"><title>Live</title>
<div><button type="button" onclick="document.getElementById('out').textContent='Stopped'">Stop</button><span id="clock">0</span></div>
<ul><li><span id="who">Customer A</span> <button type="button" onclick="document.getElementById('out').textContent='Deleted '+document.getElementById('who').textContent">Delete</button></li></ul>
<p id="out"></p>
<script>let n=0;setInterval(()=>{document.getElementById('clock').textContent=String(++n)},20);
let done=false;setInterval(async()=>{if(!done&&(await(await fetch('/swap')).json())){done=true;document.getElementById('who').textContent='Customer B';await fetch('/swapped',{method:'POST'});}},20);</script>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_reference_survives_changes_beside_it_but_not_to_its_record() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-live-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let swap = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/live", get(|| async { Html(LIVE) }))
        .route(
            "/swap",
            get(|State(state): State<Arc<AtomicUsize>>| async move {
                Json(state.load(Ordering::Acquire) > 0)
            }),
        )
        .route(
            "/swapped",
            post(|State(state): State<Arc<AtomicUsize>>| async move {
                state.store(2, Ordering::Release);
                "ok"
            }),
        )
        .with_state(swap.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/live")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();

    // The clock beside the button ticks several times before the click arrives.
    let observed = peer.observe_named(&id, 1, "Stop").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stopped = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Stop")}})).await;
    assert_eq!(stopped["result"]["input_acknowledged"], true, "{stopped}");
    let shown = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Stopped"}),
        )
        .await;
    assert_eq!(shown["result"]["condition_observed"], true, "{shown}");

    // The row a control belongs to is part of what its reference means.
    let observed = peer.observe_named(&id, 1, "Delete").await;
    assert!(observed.to_string().contains("Customer A"), "{observed}");
    swap.store(1, Ordering::Release);
    for _ in 0..200 {
        if swap.load(Ordering::Acquire) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(swap.load(Ordering::Acquire), 2);
    let stale = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Delete")}})).await;
    assert_eq!(stale["error_code"], "browser_stale_reference", "{stale}");
    assert_eq!(stale["execution_state"], "not_sent", "{stale}");
    let observed = peer.observe_named(&id, 1, "Delete").await;
    assert!(observed.to_string().contains("Customer B"), "{observed}");
    assert!(!observed.to_string().contains("Deleted"), "{observed}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_observation_says_when_the_document_is_still_loading() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-loading-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    // The page's script is withheld, which stops the parser between the two paragraphs.
    let release = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route("/held", get(|| async { Html("<!doctype html><meta charset=\"utf-8\"><title>Held</title><p>Before the script</p><script src=\"/hold.js\"></script><p>After the script</p>") }))
        .route("/hold.js", get(|State(release): State<Arc<AtomicBool>>| async move {
            while !release.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            ""
        }))
        .with_state(release.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/held")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();

    let partial = peer.observe_named(&id, 1, "Before the script").await;
    assert_eq!(partial["page"]["ready_state"], "loading", "{partial}");
    assert_eq!(
        partial["coverage"]["complete_within_scope"], false,
        "{partial}"
    );
    assert_eq!(
        partial["coverage"]["omitted"],
        json!(["document_loading"]),
        "{partial}"
    );
    assert!(
        !partial.to_string().contains("After the script"),
        "{partial}"
    );

    release.store(true, Ordering::Release);
    let whole = peer.observe_named(&id, 1, "After the script").await;
    assert_ne!(whole["page"]["ready_state"], "loading", "{whole}");
    assert_eq!(whole["coverage"]["complete_within_scope"], true, "{whole}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_navigation_that_gets_no_answer_leaves_the_session_usable() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-silent-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route(
        "/plain",
        get(|| async { Html("<!doctype html><title>Plain</title><p>Still here</p>") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // A host that takes the connection and never answers: bound, never accepted.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unanswered = format!("http://{}/", silent.local_addr().unwrap());
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/plain")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    peer.observe_named(&id, 1, "Still here").await;

    // The Agent gives up within the call and says so itself, instead of leaving
    // the caller to run into the response deadline.
    peer.budget = 4000;
    let failed = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":unanswered}})).await;
    peer.budget = 15000;
    assert_eq!(
        failed["error_code"], "browser_navigation_failed",
        "{failed}"
    );
    assert_eq!(failed["execution_state"], "unknown", "{failed}");

    // The session is paused, as after any uncertain navigation, and nothing is
    // left pending: the first resume works and the earlier document is intact.
    let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
    assert_eq!(tabs["result"]["controller"], "paused", "{tabs}");
    assert_eq!(tabs["result"]["input_pending"], false, "{tabs}");
    let resumed = peer.call("browser_control", json!({"session_id":id,"control_epoch":tabs["result"]["control_epoch"],"action":"resume"})).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let epoch = resumed["result"]["control_epoch"].as_u64().unwrap();
    let observed = peer.observe_named(&id, epoch, "Still here").await;
    assert_eq!(
        observed["page"]["url"],
        format!("{origin}/plain"),
        "{observed}"
    );

    drop(silent);
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

/// `count` buttons, each in its own list item, named `<label> 00` onwards.
fn buttons(label: &str, count: usize) -> String {
    (0..count)
        .map(|i| format!("<li>Entry {i:02} <button type=\"button\">{label} {i:02}</button></li>"))
        .collect()
}

/// One observation call, which must succeed as it is.
async fn observe_once(peer: &mut Peer, id: &str, epoch: u64) -> Value {
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":epoch}),
        )
        .await;
    assert!(reply.get("error").is_none(), "{reply}");
    reply["result"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_observation_reads_a_page_that_keeps_changing() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-changing-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    // A clock and a banner change as often as the page gets a turn: each change
    // posts itself the message that causes the next one. That is far more often
    // than a timer allows, and more often than the calls of a read follow each
    // other, however fast a page is read.
    let busy = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Busy</title>
<h1>Orders</h1><p id="clock">tick 0</p><ul>{}</ul><div id="banner" class="a">Deals</div>
<button type="button" onclick="document.getElementById('out').textContent='Order saved'">Save order</button><p id="out"></p>
<script>let n=0;const turn=new MessageChannel();turn.port1.onmessage=()=>{{n++;document.getElementById('clock').textContent='tick '+n;document.getElementById('banner').className=n%2?'a':'b';turn.port2.postMessage(0);}};turn.port2.postMessage(0);</script>"#,
        buttons("Open", 40)
    );
    // The last row of the table shows a new stock figure in the same way, until
    // the feed is paused. The columns keep their width, so no row moves.
    let rows = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Rows</title><h1>Stock</h1>
<table style="table-layout:fixed;width:480px"><tbody>{}<tr><td id="live">Stock 0</td><td><button type="button">Buy live</button></td></tr></tbody></table>
<button type="button" onclick="feeding=false">Pause feed</button><p id="out"></p>
<script>let n=0,feeding=true;const turn=new MessageChannel();turn.port1.onmessage=()=>{{if(!feeding)return;document.getElementById('live').textContent='Stock '+(++n);turn.port2.postMessage(0);}};turn.port2.postMessage(0);</script>"#,
        (0..40)
            .map(|i| format!("<tr><td>Item {i:02}</td><td><button type=\"button\" onclick=\"document.getElementById('out').textContent='Bought item {i:02}'\">Buy {i:02}</button></td></tr>"))
            .collect::<String>()
    );
    // Every text node of the first block and every element of the second is
    // replaced, by one that reads the same, every few milliseconds.
    let replacing = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Replacing</title><div id="texts">{}</div><div id="rows">{}</div><p>Tail reached</p>
<script>setInterval(()=>{{for(const p of document.querySelectorAll('#texts p'))p.textContent=p.textContent;const rows=document.getElementById('rows');rows.innerHTML=rows.innerHTML;}},10);</script>"#,
        (0..12)
            .map(|i| format!("<p>Text {i:02}</p>"))
            .collect::<String>(),
        (0..12)
            .map(|i| format!("<p>Row {i:02}</p>"))
            .collect::<String>()
    );
    let app = Router::new()
        .route("/busy", get(move || async move { Html(busy) }))
        .route("/rows", get(move || async move { Html(rows) }))
        .route("/replacing", get(move || async move { Html(replacing) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/busy")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let wait = |text: &str| json!({"session_id":id,"control_epoch":1,"text_contains":text});
    let navigate = |path: &str| json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/{path}")}});

    // The whole page is read in one call while it changes, every control gets a
    // reference, and the coverage says that what was read changed meanwhile.
    let loaded = peer.call("browser_wait", wait("Save order")).await;
    assert_eq!(loaded["result"]["condition_observed"], true, "{loaded}");
    let observed = observe_once(&mut peer, &id, 1).await;
    let controls = observed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["role"] == "button" && n["ref"].is_string())
        .count();
    assert_eq!(controls, 41, "{observed}");
    let coverage = &observed["coverage"];
    assert_eq!(coverage["more"], false, "{observed}");
    assert_eq!(coverage["changed_during_read"], true, "{observed}");
    assert_eq!(coverage["complete_within_scope"], false, "{observed}");
    assert_eq!(coverage["omitted"], json!([]), "{observed}");
    let saved = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Save order")}})).await;
    assert_eq!(saved["result"]["input_acknowledged"], true, "{saved}");
    let shown = peer.call("browser_wait", wait("Order saved")).await;
    assert_eq!(shown["result"]["condition_observed"], true, "{shown}");

    // A control whose row changed after it was reported gets no reference, since
    // the reference would be bound to a row other than the reported one. The
    // other rows are unaffected.
    let moved = peer.call("browser_act", navigate("rows")).await;
    assert!(moved.get("error").is_none(), "{moved}");
    let loaded = peer.call("browser_wait", wait("Pause feed")).await;
    assert_eq!(loaded["result"]["condition_observed"], true, "{loaded}");
    let observed = observe_once(&mut peer, &id, 1).await;
    let live = node_named(&observed, "Buy live");
    assert!(live.get("ref").is_none(), "{observed}");
    assert_eq!(live["changed_during_read"], true, "{observed}");
    let still = node_named(&observed, "Buy 07");
    assert!(still.get("changed_during_read").is_none(), "{observed}");
    assert_eq!(
        observed["coverage"]["changed_during_read"], true,
        "{observed}"
    );
    let bought = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Buy 07")}})).await;
    assert_eq!(bought["result"]["input_acknowledged"], true, "{bought}");
    let shown = peer.call("browser_wait", wait("Bought item 07")).await;
    assert_eq!(shown["result"]["condition_observed"], true, "{shown}");
    // Once the row holds still, its control has a reference and the read is complete.
    let observed = observe_once(&mut peer, &id, 1).await;
    let paused = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Pause feed")}})).await;
    assert_eq!(paused["result"]["input_acknowledged"], true, "{paused}");
    let observed = observe_once(&mut peer, &id, 1).await;
    assert!(
        node_named(&observed, "Buy live")["ref"].is_string(),
        "{observed}"
    );
    assert_eq!(
        observed["coverage"]["changed_during_read"], false,
        "{observed}"
    );
    assert_eq!(
        observed["coverage"]["complete_within_scope"], true,
        "{observed}"
    );

    // A sequence read one node at a time stops on a node that the page replaces
    // before the next call. It goes on from the same place, reads every row once
    // and reaches the end.
    let moved = peer.call("browser_act", navigate("replacing")).await;
    assert!(moved.get("error").is_none(), "{moved}");
    let loaded = peer.call("browser_wait", wait("Tail reached")).await;
    assert_eq!(loaded["result"]["condition_observed"], true, "{loaded}");
    let mut names = Vec::new();
    let mut cursor = Value::Null;
    let mut last = Value::Null;
    for _ in 0..40 {
        let mut params = json!({"session_id":id,"control_epoch":1,"max_nodes":1});
        if !cursor.is_null() {
            params["cursor"] = cursor.clone();
        }
        let reply = peer.call("browser_observe", params).await;
        assert!(reply.get("error").is_none(), "{reply}");
        names.extend(names_of(&reply["result"]));
        cursor = reply["result"]["next_cursor"].clone();
        last = reply["result"].clone();
        if cursor.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let expected: Vec<String> = (0..12)
        .map(|i| format!("Text {i:02}"))
        .chain((0..12).map(|i| format!("Row {i:02}")))
        .chain(["Tail reached".to_owned()])
        .collect();
    assert_eq!(names, expected);
    assert_eq!(last["coverage"]["more"], false, "{last}");
    assert_eq!(last["coverage"]["changed_during_read"], true, "{last}");
    assert_eq!(last["coverage"]["sequence_complete"], false, "{last}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_read_outlasts_a_subframe_that_navigates() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-subframe-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    // Once the page has loaded, it reloads its subframe every few milliseconds.
    let frames = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Frames</title><h1>Frames</h1><ul>{}</ul><iframe id="inner" src="/inner"></iframe>
<script>addEventListener('load',()=>setInterval(()=>document.getElementById('inner').contentWindow.location.reload(),15));</script>"#,
        buttons("Keep", 60)
    );
    let app = Router::new()
        .route("/frames", get(move || async move { Html(frames) }))
        .route(
            "/inner",
            get(|| async { Html("<!doctype html><title>Inner</title><p>Frame body</p>") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/frames")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();

    // The subframe is a different document by the time the read comes to it. It
    // is left out and named; the tab's own document is read in full.
    let loaded = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Keep 59"}),
        )
        .await;
    assert_eq!(loaded["result"]["condition_observed"], true, "{loaded}");
    let observed = observe_once(&mut peer, &id, 1).await;
    assert!(
        node_named(&observed, "Keep 00")["ref"].is_string(),
        "{observed}"
    );
    assert!(
        node_named(&observed, "Keep 59")["ref"].is_string(),
        "{observed}"
    );
    let coverage = &observed["coverage"];
    assert_eq!(coverage["omitted"], json!(["frame_changed"]), "{observed}");
    assert_eq!(coverage["complete_within_scope"], false, "{observed}");
    assert_eq!(coverage["changed_during_read"], false, "{observed}");
    assert_eq!(coverage["more"], false, "{observed}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_first_read_follows_a_document_that_is_replaced_under_it() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-replaced-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    // The page leaves for another one as soon as the test says so.
    let leaving = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Leaving</title><h1>Leaving</h1><ul>{}</ul>
<script>const poll=setInterval(async()=>{{if(await(await fetch('/go')).json()){{clearInterval(poll);location.href='/arrived';}}}},5);</script>"#,
        buttons("Stay", 150)
    );
    let go = Arc::new(AtomicBool::new(false));
    let app = Router::new()
        .route("/leaving", get(move || async move { Html(leaving) }))
        .route(
            "/arrived",
            get(|| async { Html("<!doctype html><title>Arrived</title><h1>Arrived</h1>") }),
        )
        .route(
            "/go",
            get(
                |State(go): State<Arc<AtomicBool>>| async move { Json(go.load(Ordering::Acquire)) },
            ),
        )
        .with_state(go.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/leaving")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    peer.observe_named(&id, 1, "Stay 00").await;

    // Reading this page takes far longer than the page takes to leave, so the
    // document is replaced while the read is under way. The same call reads the
    // document that took its place.
    go.store(true, Ordering::Release);
    let observed = observe_once(&mut peer, &id, 1).await;
    assert_eq!(
        observed["page"]["url"],
        format!("{origin}/arrived"),
        "{observed}"
    );
    assert!(!observed.to_string().contains("Stay"), "{observed}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

/// Polls `browser_tabs` until a tab matches, and returns the tabs reply.
async fn tabs_until(peer: &mut Peer, id: &str, found: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..60 {
        last = peer.call("browser_tabs", json!({"session_id":id})).await;
        if last["result"]["tabs"]
            .as_array()
            .is_some_and(|tabs| tabs.iter().any(&found))
        {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no such tab: {last}");
}

const METADATA: &str = "http://169.254.169.254/latest/meta-data/";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn metadata_addresses_are_refused_however_the_browser_is_led_to_them() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-sites-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let home = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Home</title><h1>Home</h1>
<a href="{METADATA}">Metadata link</a> <a href="{METADATA}" target="_blank">Popup link</a>"#
    );
    let framed = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Framed</title><h1>Framed</h1><iframe src="{METADATA}"></iframe><p>After frame</p>"#
    );
    let app = Router::new()
        .route("/", get(move || async move { Html(home) }))
        .route("/framed", get(move || async move { Html(framed) }))
        .route(
            "/redirect",
            get(|| async { axum::response::Redirect::temporary(METADATA) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // The policy says nothing about sites: this device and private networks are open.
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    peer.observe_named(&id, 1, "Home").await;
    let navigate = |epoch: u64, url: String| json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":url}});

    // An address the caller gives is refused before the browser does anything:
    // nothing is sent, the session keeps running and the tab keeps its page.
    for refused in [
        METADATA,
        "http://2852039166/",
        "http://100.100.100.200/",
        "http://metadata.google.internal/computeMetadata/v1/",
    ] {
        let denied = peer
            .call("browser_act", navigate(1, refused.to_owned()))
            .await;
        assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
        assert_eq!(denied["execution_state"], "not_sent", "{denied}");
    }
    assert_running(&mut peer, &id).await;
    let observed = observe_once(&mut peer, &id, 1).await;
    assert_eq!(
        node_named(&observed, "Home")["role"],
        "heading",
        "{observed}"
    );
    assert!(observed["page"].get("blocked").is_none(), "{observed}");
    // No session is started for such an address either.
    let denied = peer.call("browser_open", json!({"url":METADATA})).await;
    assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
    assert_eq!(denied["execution_state"], "not_sent", "{denied}");
    let sessions = peer.call("browser_sessions", json!({})).await;
    assert_eq!(
        sessions["result"]["sessions"].as_array().unwrap().len(),
        1,
        "{sessions}"
    );

    // A link leads there. The click itself succeeds; the navigation is refused,
    // and the tab says why it shows nothing.
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Metadata link")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let tabs = tabs_until(&mut peer, &id, |tab| tab["blocked"] == "site_policy").await;
    assert_eq!(tabs["result"]["controller"], "ai", "{tabs}");
    let observed = observe_once(&mut peer, &id, 1).await;
    assert_eq!(observed["page"]["blocked"], "site_policy", "{observed}");
    assert!(!observed.to_string().contains("Home"), "{observed}");

    // Setting out for a page that may be opened clears the note.
    let moved = peer
        .call("browser_act", navigate(1, format!("{origin}/framed")))
        .await;
    assert!(moved.get("error").is_none(), "{moved}");
    // A frame that points there is refused; the page around it is read as usual.
    let observed = peer.observe_named(&id, 1, "After frame").await;
    assert!(observed["page"].get("blocked").is_none(), "{observed}");

    // A link that opens a new tab: the new tab is refused, not the one clicked in.
    let moved = peer
        .call("browser_act", navigate(1, format!("{origin}/")))
        .await;
    assert!(moved.get("error").is_none(), "{moved}");
    let observed = peer.observe_named(&id, 1, "Home").await;
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Popup link")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let tabs = tabs_until(&mut peer, &id, |tab| tab["blocked"] == "site_policy").await;
    let listed = tabs["result"]["tabs"].as_array().unwrap();
    assert_eq!(listed.len(), 2, "{tabs}");
    let first = listed
        .iter()
        .find(|tab| tab["tab_id"] == opened["result"]["tab_id"])
        .unwrap();
    assert!(first.get("blocked").is_none(), "{tabs}");

    // A page the caller may open redirects there. A request has left the browser
    // by then, so the outcome is uncertain and the session pauses, as after any
    // navigation that fails part of the way.
    let denied = peer
        .call("browser_act", navigate(1, format!("{origin}/redirect")))
        .await;
    assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
    assert_eq!(denied["execution_state"], "unknown", "{denied}");
    let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
    assert_eq!(tabs["result"]["controller"], "paused", "{tabs}");
    let resumed = peer.call("browser_control", json!({"session_id":id,"control_epoch":tabs["result"]["control_epoch"],"action":"resume"})).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let epoch = resumed["result"]["control_epoch"].as_u64().unwrap();
    let observed = observe_once(&mut peer, &id, epoch).await;
    assert_eq!(observed["page"]["blocked"], "site_policy", "{observed}");

    // A name that leads to such an address is refused once that is known, before
    // its request is sent. The test says where names lead, in place of the
    // system's resolver.
    peer.runtime
        .browser_resolver(rdh_agent::browser::sites::resolver(
            |host: String| async move {
                (host == "metadata.test").then(|| vec!["169.254.169.254".parse().unwrap()])
            },
        ));
    let denied = peer
        .call(
            "browser_act",
            navigate(epoch, "http://metadata.test/".to_owned()),
        )
        .await;
    assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
    assert_eq!(denied["execution_state"], "not_sent", "{denied}");
    let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
    assert_eq!(tabs["result"]["controller"], "ai", "{tabs}");

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

/// A server on `address` whose every page shows `title`, and the number of
/// requests it has received.
async fn counting_site(
    address: std::net::IpAddr,
    title: &'static str,
) -> (
    u16,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().fallback(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        async move {
            Html(format!(
                "<!doctype html><title>{title}</title><h1>{title}</h1>"
            ))
        }
    });
    let listener = tokio::net::TcpListener::bind((address, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, hits, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_owner_who_refuses_private_networks_names_the_local_sites_that_open() {
    use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
    use std::sync::atomic::Ordering;
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-strict-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let (refused_port, refused_hits, refused_site) = counting_site(loopback, "Refused page").await;
    let linked = format!("http://127.0.0.1:{refused_port}/");
    let (home, framed, redirect) = (
        format!(
            r#"<!doctype html><meta charset="utf-8"><title>Allowed</title><h1>Allowed</h1><a href="{linked}">Refused link</a>"#
        ),
        format!(
            r#"<!doctype html><meta charset="utf-8"><title>Framed</title><h1>Framed</h1><iframe src="{linked}"></iframe><p>After frame</p>"#
        ),
        linked.clone(),
    );
    let app = Router::new()
        .route("/", get(move || async move { Html(home) }))
        .route("/framed", get(move || async move { Html(framed) }))
        .route(
            "/redirect",
            get(move || async move { axum::response::Redirect::temporary(&redirect) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let allowed_port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{allowed_port}");
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // This machine's own name leads to an address of its own, which is not public.
    // Two sites answer under that name: one the owner lists, one the owner does not.
    let name = String::from_utf8(
        std::process::Command::new("hostname")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_ascii_lowercase();
    let own = (name.as_str(), 0)
        .to_socket_addrs()
        .ok()
        .and_then(|mut found| found.find(|address| address.is_ipv4()))
        .map(|address| address.ip());
    let named = match own {
        Some(address) => Some((
            counting_site(address, "Listed page").await,
            counting_site(address, "Unlisted page").await,
        )),
        None => None,
    };
    let mut listed = vec![format!("127.0.0.1:{allowed_port}")];
    if let Some(((port, ..), _)) = &named {
        listed.push(format!("{name}:{port}"));
    }
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"downloads":true,"executable":executable,
            "sites":{"private_networks":"deny","private_allow":listed}}),
    );
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let mut epoch = 1;
    let navigate = |epoch: u64, url: String| json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":url}});
    // The session pauses after an uncertain outcome; this takes control back.
    async fn resume(peer: &mut Peer, id: &str) -> u64 {
        let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
        assert_eq!(tabs["result"]["controller"], "paused", "{tabs}");
        let resumed = peer.call("browser_control", json!({"session_id":id,"control_epoch":tabs["result"]["control_epoch"],"action":"resume"})).await;
        assert!(resumed.get("error").is_none(), "{resumed}");
        resumed["result"]["control_epoch"].as_u64().unwrap()
    }

    // The listed local site opens. Another site on this device does not, whether
    // the caller names it, a link leads to it, a frame points at it or a listed
    // page redirects to it. No request reaches it.
    let observed = peer.observe_named(&id, epoch, "Allowed").await;
    let denied = peer
        .call("browser_act", navigate(epoch, linked.clone()))
        .await;
    assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
    assert_eq!(denied["execution_state"], "not_sent", "{denied}");
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"click","ref":reference(&observed,"Refused link")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    tabs_until(&mut peer, &id, |tab| tab["blocked"] == "site_policy").await;
    let moved = peer
        .call("browser_act", navigate(epoch, format!("{origin}/framed")))
        .await;
    assert!(moved.get("error").is_none(), "{moved}");
    let observed = peer.observe_named(&id, epoch, "After frame").await;
    assert!(!observed.to_string().contains("Refused page"), "{observed}");
    let denied = peer
        .call("browser_act", navigate(epoch, format!("{origin}/redirect")))
        .await;
    assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
    assert_eq!(denied["execution_state"], "unknown", "{denied}");
    epoch = resume(&mut peer, &id).await;
    assert_eq!(refused_hits.load(Ordering::Relaxed), 0);

    if let Some(((listed_port, ..), (unlisted_port, unlisted_hits, _))) = &named {
        // A local site listed by name opens.
        let moved = peer
            .call(
                "browser_act",
                navigate(epoch, format!("http://{name}:{listed_port}/")),
            )
            .await;
        assert!(moved.get("error").is_none(), "{moved}");
        peer.observe_named(&id, epoch, "Listed page").await;
        // An unlisted name is looked up before its request is sent. The lookup
        // finds an address that is not public, and nothing is sent.
        let unlisted = format!("http://{name}:{unlisted_port}/");
        let denied = peer
            .call("browser_act", navigate(epoch, unlisted.clone()))
            .await;
        assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
        assert_eq!(denied["execution_state"], "not_sent", "{denied}");
        assert_eq!(unlisted_hits.load(Ordering::Relaxed), 0);
        // Now the lookup is told that every name is public, as it would see a name
        // that answers the Agent and the browser differently. The request is sent,
        // the browser reaches an address that is not public, and the page never
        // gets the response.
        peer.runtime
            .browser_resolver(rdh_agent::browser::sites::resolver(|_| async {
                Some(vec!["93.184.216.34".parse().unwrap()])
            }));
        let denied = peer.call("browser_act", navigate(epoch, unlisted)).await;
        assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
        assert_eq!(denied["execution_state"], "unknown", "{denied}");
        assert_eq!(unlisted_hits.load(Ordering::Relaxed), 1);
        epoch = resume(&mut peer, &id).await;
        let observed = observe_once(&mut peer, &id, epoch).await;
        assert_eq!(observed["page"]["blocked"], "site_policy", "{observed}");
        assert!(
            !observed.to_string().contains("Unlisted page"),
            "{observed}"
        );
    } else {
        eprintln!("this machine's name does not resolve: the checks by name were skipped");
    }

    assert!(peer.runtime.shutdown().await);
    for site in [server, refused_site] {
        site.abort();
        let _ = site.await;
    }
    if let Some(((_, _, listed), (_, _, unlisted))) = named {
        listed.abort();
        unlisted.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn only_the_public_sites_an_owner_lists_open_when_the_rest_are_refused() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-listed-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(r#"<!doctype html><meta charset="utf-8"><title>Local</title><h1>Local</h1><a href="http://unlisted.test/">Unlisted link</a>"#)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"executable":executable,
            "sites":{"public":"deny","allow":["listed.test","*.listed.test"],"deny":["no.listed.test"]}}),
    );
    // Every name is public as far as the lookup knows.
    peer.runtime
        .browser_resolver(rdh_agent::browser::sites::resolver(|_| async {
            Some(vec!["93.184.216.34".parse().unwrap()])
        }));
    // The list of public sites does not close this device's own.
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let observed = peer.observe_named(&id, 1, "Local").await;
    let navigate = |url: &str| json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":url}});

    for refused in [
        "https://unlisted.test/",
        "https://notlisted.test/",
        "https://no.listed.test/",
        "https://93.184.216.34/",
    ] {
        let denied = peer.call("browser_act", navigate(refused)).await;
        assert_eq!(denied["error_code"], "browser_site_denied", "{denied}");
        assert_eq!(denied["execution_state"], "not_sent", "{denied}");
    }
    assert_running(&mut peer, &id).await;
    // A link to an unlisted site is refused as well.
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Unlisted link")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    tabs_until(&mut peer, &id, |tab| tab["blocked"] == "site_policy").await;
    // A listed site passes the policy. These names do not exist, so the browser
    // then fails to find them, which is a different failure and pauses the session.
    let mut epoch = 1;
    for listed in ["http://listed.test/", "http://docs.listed.test/"] {
        let failed = peer.call("browser_act", json!({"session_id":id,"control_epoch":epoch,"action":{"kind":"navigate","url":listed}})).await;
        assert_eq!(
            failed["error_code"], "browser_navigation_failed",
            "{failed}"
        );
        let tabs = peer.call("browser_tabs", json!({"session_id":id})).await;
        assert!(tabs["result"]["tabs"][0].get("blocked").is_none(), "{tabs}");
        let resumed = peer.call("browser_control", json!({"session_id":id,"control_epoch":tabs["result"]["control_epoch"],"action":"resume"})).await;
        assert!(resumed.get("error").is_none(), "{resumed}");
        epoch = resumed["result"]["control_epoch"].as_u64().unwrap();
    }

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_device_with_a_shell_offers_browser_sessions_as_its_owner_decided() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-shell-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html("<!doctype html><title>Beside a shell</title><h1>Beside a shell</h1>")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let policy = |coexistence: Option<&str>, sandbox: bool| {
        let mut policy = json!({"version":1,"browser":{"enabled":true,"executable":executable}});
        if let Some(coexistence) = coexistence {
            policy["browser"]["shell_coexistence"] = coexistence.into();
        }
        if sandbox {
            policy["sandbox"] = json!({"mode":"bwrap","network":true,"hide_home":false});
        }
        policy
    };
    let browser = |peer: &Peer| peer.runtime.info()["capabilities"]["browser"].clone();

    // Nothing said: a shell and browser sessions go together, as they did before.
    let mut peer = device_peer(&temp.0, policy(None, false), true).unwrap();
    let report = browser(&peer);
    assert_eq!(report["available"], true, "{report}");
    assert_eq!(
        report["shell"],
        json!({"enabled":true,"sandboxed":false,"coexistence":"allow"}),
        "{report}"
    );
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    peer.observe_named(&id, 1, "Beside a shell").await;
    assert!(peer.runtime.shutdown().await);

    // Refused together: the shell stays, and no browser session is opened.
    for (coexistence, reason) in [
        ("deny", "shell_enabled"),
        ("sandboxed_only", "shell_not_sandboxed"),
    ] {
        let mut peer = device_peer(&temp.0, policy(Some(coexistence), false), true).unwrap();
        let report = browser(&peer);
        assert_eq!(report["available"], false, "{report}");
        assert_eq!(report["reason"], reason, "{report}");
        assert_eq!(peer.runtime.info()["allow_shell"], true);
        let refused = peer
            .call("browser_open", json!({"url":format!("{origin}/")}))
            .await;
        assert_eq!(refused["error_code"], "browser_unavailable", "{refused}");
        assert_eq!(refused["execution_state"], "not_sent", "{refused}");
        assert!(peer.runtime.shutdown().await);
        // The same policy on a device without a shell leaves browser sessions alone.
        let peer = device_peer(&temp.0, policy(Some(coexistence), false), false).unwrap();
        let report = browser(&peer);
        assert_eq!(report["available"], true, "{report}");
        assert_eq!(report["shell"]["enabled"], false, "{report}");
        assert!(peer.runtime.shutdown().await);
    }

    // With the shell inside the sandbox, the stricter of the two that allow both
    // is met. A device without bubblewrap cannot start with that policy at all.
    match device_peer(&temp.0, policy(Some("sandboxed_only"), true), true) {
        Ok(mut peer) => {
            let report = browser(&peer);
            assert_eq!(report["available"], true, "{report}");
            assert_eq!(
                report["shell"],
                json!({"enabled":true,"sandboxed":true,"coexistence":"sandboxed_only"}),
                "{report}"
            );
            let opened = peer
                .call("browser_open", json!({"url":format!("{origin}/")}))
                .await;
            assert!(opened.get("error").is_none(), "{opened}");
            let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
            peer.observe_named(&id, 1, "Beside a shell").await;
            assert!(peer.runtime.shutdown().await);
        }
        Err(error) if error.contains("bubblewrap") => {
            eprintln!("bubblewrap is unavailable: the sandboxed case was skipped");
        }
        Err(error) => panic!("{error}"),
    }

    server.abort();
    let _ = server.await;
}

// Every sort of control a page can hold: the ones HTML defines, the ones a role
// attribute declares, and elements that only their handler, their cursor or
// their place in the focus order mark as taking clicks. The last part holds
// what must not be taken for a control.
const CONTROLS: &str = r##"<!doctype html><meta charset="utf-8"><title>Controls</title>
<style>.hand{cursor:pointer} table{table-layout:fixed;width:600px}</style>
<h1>Controls</h1>
<p><a href="#here">Plain link</a> <a>Anchor without address</a> <button type="button">Plain button</button></p>
<p><label>Name <input value="Ada"></label> <label>Tint <input type="color" value="#336699"></label>
<label>Day <input type="date" value="2026-01-02"></label> <label>Level <input type="range" min="0" max="10" value="4"></label>
<input type="hidden" name="token" value="t"></p>
<p><label>Notes <textarea>draft</textarea></label> <label>Size <select><option>Small</option><option>Large</option></select></label>
<label>Tags <select multiple size="2"><option>Red</option><option>Blue</option></select></label></p>
<details><summary>More options</summary><p>Shown when opened</p></details>
<div contenteditable="true">Edit me <b>boldly</b></div>
<div contenteditable="true" role="textbox" aria-label="Labelled editor">Type here</div>
<div role="button" tabindex="0">Role button</div>
<div role="grid" aria-label="Sheet"><div role="row"><div role="gridcell" tabindex="-1">Grid cell</div></div></div>
<ul role="tree" aria-label="Files"><li role="treeitem" tabindex="0">Tree item</li></ul>
<ul role="menu" aria-label="View"><li role="menuitemcheckbox" aria-checked="true" tabindex="0">Show grid</li><li role="menuitemradio" aria-checked="false" tabindex="-1">Compact</li><li role="menuitem" tabindex="-1">Reset view</li></ul>
<div role="tablist"><div role="tab" aria-selected="true" tabindex="0">First tab</div></div>
<p onclick="note('paragraph')">Paragraph with handler</p>
<p id="scripted">Paragraph with listener</p>
<div class="hand">Hand cursor block <span>with inner text</span></div>
<div tabindex="0">Focusable block</div>
<table><thead><tr><th class="hand" onclick="note('header')">Sortable header</th><th>Plain header</th></tr></thead>
<tbody><tr><td>Plain cell</td><td class="hand">Clickable cell</td></tr></tbody></table>
<svg class="hand" width="20" height="20" viewBox="0 0 20 20"><title>Icon button</title><circle cx="10" cy="10" r="8"/></svg>
<img class="hand" alt="Picture button" width="20" height="20" src="data:image/gif;base64,R0lGODlhAQABAAAAACw=">
<a class="hand" onclick="note('anchor')">Anchor with handler</a>
<a id="wired">Anchor with listener</a>
<i class="hand" aria-hidden="true" style="display:inline-block;width:16px;height:16px"></i>
<pre tabindex="0" style="overflow:auto;max-height:40px">Scrollable code</pre>
<div role="tabpanel" tabindex="0">Panel text</div>
<div role="region" aria-label="Notes">Region text</div>
<p>Plain paragraph</p>
<a href="#inner" class="hand"><span class="hand">Link with inner span</span></a>
<button type="button"><span style="cursor:pointer">Button with inner span</span></button>
<div class="hand"><a href="#only">Wrapped link</a></div>
<p id="out"></p>
<script>function note(what){document.getElementById('out').textContent='Noted '+what;}
for(const id of ['scripted','wired'])document.getElementById(id).addEventListener('click',()=>note(id));</script>"##;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn controls_have_references_by_what_the_page_makes_of_them() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-controls-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/controls", get(|| async { Html(CONTROLS) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/controls")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let observed = peer.observe_named(&id, 1, "Plain paragraph").await;
    let nodes = observed["nodes"].as_array().unwrap();
    // The browser names a field after its label, and keeps the space around it.
    let named = |name: &str| -> Vec<&Value> {
        nodes
            .iter()
            .filter(|n| n["name"].as_str().is_some_and(|s| s.trim() == name))
            .collect()
    };
    let referenced = |name: &str| -> &Value {
        let found: Vec<&Value> = named(name)
            .into_iter()
            .filter(|n| n.get("ref").is_some())
            .collect();
        assert_eq!(found.len(), 1, "{name}: {observed}");
        found[0]
    };

    // Each control has one reference, and says which kind of control it is. The
    // kind is the same on every browser version, whatever the browser calls the
    // element.
    for (kind, names) in [
        (
            "native",
            &[
                "Plain link",
                "Plain button",
                "Name",
                "Tint",
                "Day",
                "Level",
                "Notes",
                "Size",
                "Tags",
                "Red",
                "More options",
                "Labelled editor",
                "Link with inner span",
                "Button with inner span",
                "Wrapped link",
                // The markup does not make a control of this anchor. The browser
                // knows of the click handler that a script gave it, and calls it
                // a link.
                "Anchor with listener",
            ][..],
        ),
        (
            "aria",
            &[
                "Role button",
                "Grid cell",
                "Tree item",
                "Show grid",
                "Compact",
                "Reset view",
                "First tab",
            ][..],
        ),
        (
            "inferred",
            &[
                "Paragraph with handler",
                "Hand cursor block with inner text",
                "Focusable block",
                "Sortable header",
                "Clickable cell",
                "Icon button",
                "Picture button",
                "Anchor with handler",
            ][..],
        ),
    ] {
        for name in names {
            assert_eq!(referenced(name)["kind"], kind, "{name}: {observed}");
        }
    }
    // An editable region without a role has no name. It is a field all the same.
    let editors: Vec<&Value> = nodes
        .iter()
        .filter(|n| n["kind"] == "native" && n["name"] == "")
        .collect();
    assert_eq!(editors.len(), 1, "{observed}");
    assert_eq!(editors[0]["editable"], true, "{observed}");
    assert!(editors[0].get("ref").is_some(), "{observed}");
    // A colour and a date are read like the value of any other field.
    assert_eq!(referenced("Tint")["value"], "#336699", "{observed}");
    assert_eq!(referenced("Day")["value"], "2026-01-02", "{observed}");

    // What the page does not mark is text, without a reference: an anchor that
    // leads nowhere, plain cells, a block that is in the focus order so that it
    // can scroll, a panel. So is a paragraph whose click handler only a script
    // attaches, which the markup does not show.
    for name in [
        "Anchor without address",
        "Paragraph with listener",
        "Plain header",
        "Plain cell",
        "Scrollable code",
        "Panel text",
        "Region text",
        "Plain paragraph",
        "Shown when opened",
    ] {
        let found = named(name);
        assert_eq!(found.len(), 1, "{name}: {observed}");
        assert!(
            found[0].get("ref").is_none() && found[0].get("kind").is_none(),
            "{name}: {observed}"
        );
    }
    // Nothing else looks like a control. A block that only wraps a link is not
    // one, and neither is anything inside a link or a button.
    // The ninth is an icon with a hand cursor that shows no text and is hidden
    // from assistive technology, as icon fonts usually are. It has no name, and
    // the accessibility tree leaves it out. It shows and the page marks it, so
    // it has a reference all the same.
    let inferred: Vec<&Value> = nodes.iter().filter(|n| n["kind"] == "inferred").collect();
    assert_eq!(inferred.len(), 9, "{observed}");
    let unnamed: Vec<&&Value> = inferred.iter().filter(|n| n["name"] == "").collect();
    assert_eq!(unnamed.len(), 1, "{observed}");
    assert_eq!(unnamed[0]["role"], "generic", "{observed}");
    assert!(unnamed[0].get("ref").is_some(), "{observed}");

    // A control that is named by what it contains is reported once, and its
    // text is not reported again beside it.
    for name in [
        "Role button",
        "Tree item",
        "Show grid",
        "Compact",
        "Reset view",
        "First tab",
        "More options",
        "Paragraph with handler",
        "Hand cursor block with inner text",
        "Focusable block",
        "Sortable header",
        "Clickable cell",
        "Anchor with handler",
    ] {
        assert_eq!(named(name).len(), 1, "{name}: {observed}");
    }
    assert!(named("Hand cursor block").is_empty(), "{observed}");
    assert!(named("with inner text").is_empty(), "{observed}");

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

// Two order cards that take a click anywhere on them, each with a button of its
// own, and a chip that takes keys. Nothing but a handler, a cursor or a place in
// the focus order makes controls of the cards and the chip. The middle of the
// first card is its text, and the middle of the second is its button.
const CARDS: &str = r#"<!doctype html><meta charset="utf-8"><title>Cards</title>
<style>.card{cursor:pointer;position:relative;width:300px;height:100px;margin:8px;border:1px solid #888}
.card *{position:absolute;margin:0} .chip{width:120px;border:1px solid #888}</style>
<h1>Orders</h1>
<div class="card" onclick="note('Opened order 12')"><h3 style="top:0;left:4px">Order 12</h3>
<p style="top:40px;left:0;width:300px;height:20px">Shipped to Ada</p>
<button type="button" style="right:4px;bottom:4px;width:90px;height:20px" onclick="event.stopPropagation();note('Removed order 12')">Remove 12</button></div>
<div class="card" onclick="note('Opened order 13')"><span style="top:0;left:4px">Order 13</span>
<button type="button" style="top:30px;left:100px;width:100px;height:40px" onclick="event.stopPropagation();note('Removed order 13')">Remove 13</button></div>
<div class="chip" tabindex="0" onkeydown="if(event.key==='Enter')note('Chip chosen')">Chip</div>
<p id="out">Nothing yet</p><p id="count">0 actions</p>
<script>let n=0;function note(what){document.getElementById('out').textContent=what;document.getElementById('count').textContent=++n+' actions';}</script>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_element_that_only_looks_like_a_control_takes_clicks_and_keys() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-cards-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/cards", get(|| async { Html(CARDS) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/cards")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let shows =
        |text: &'static str| json!({"session_id":id,"control_epoch":1,"text_contains":text});

    // A card is named by its own text, without the button it holds. The button
    // and the text inside the card are reported as well.
    let observed = peer.observe_named(&id, 1, "Chip").await;
    let card = node_named(&observed, "Order 12 Shipped to Ada");
    assert_eq!(card["kind"], "inferred", "{observed}");
    assert_eq!(node_named(&observed, "Remove 12")["kind"], "native");
    assert_eq!(
        node_named(&observed, "Shipped to Ada")["role"],
        "StaticText"
    );
    let clicked = peer
        .call(
            "browser_act",
            json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":card["ref"]}}),
        )
        .await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let opened = peer.call("browser_wait", shows("Opened order 12")).await;
    assert_eq!(opened["result"]["condition_observed"], true, "{opened}");

    // It takes nothing that needs a real field, box or list. Nothing is sent.
    for mut action in [
        json!({"kind":"fill","text":"x"}),
        json!({"kind":"check","checked":true}),
        json!({"kind":"select","value":"x"}),
    ] {
        let observed = peer.observe_named(&id, 1, "Chip").await;
        action["ref"] = reference(&observed, "Order 12 Shipped to Ada").into();
        let refused = peer
            .call(
                "browser_act",
                json!({"session_id":id,"control_epoch":1,"action":action}),
            )
            .await;
        assert_eq!(
            refused["error_code"], "browser_invalid_params",
            "{action}: {refused}"
        );
        assert_eq!(refused["execution_state"], "not_sent", "{refused}");
        assert_running(&mut peer, &id).await;
    }

    // A click in the middle of the second card would be taken by its Remove
    // button. It is refused instead, and the order stays.
    let observed = peer.observe_named(&id, 1, "Chip").await;
    let refused = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Order 13")}})).await;
    assert_eq!(refused["error_code"], "browser_not_actionable", "{refused}");
    assert_eq!(refused["execution_state"], "not_sent", "{refused}");
    assert_running(&mut peer, &id).await;
    let observed = peer.observe_named(&id, 1, "Chip").await;
    assert!(names_of(&observed).contains(&"Opened order 12".to_owned()));
    assert!(names_of(&observed).contains(&"1 actions".to_owned()));
    // The button is clicked by its own reference.
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&observed,"Remove 13")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let removed = peer.call("browser_wait", shows("Removed order 13")).await;
    assert_eq!(removed["result"]["condition_observed"], true, "{removed}");

    // An element in the focus order takes keys.
    let observed = peer.observe_named(&id, 1, "Chip").await;
    assert_eq!(node_named(&observed, "Chip")["kind"], "inferred");
    let pressed = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"press","key":"Enter","ref":reference(&observed,"Chip")}})).await;
    assert_eq!(pressed["result"]["input_acknowledged"], true, "{pressed}");
    let chosen = peer.call("browser_wait", shows("Chip chosen")).await;
    assert_eq!(chosen["result"]["condition_observed"], true, "{chosen}");

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

// Fields whose value is picked: typing into them does nothing.
const PICKED: &str = r##"<!doctype html><meta charset="utf-8"><title>Picked</title>
<h1>Booking</h1>
<p><label>Day <input type="date" value="2026-01-02" onchange="note('day '+this.value)"></label>
<label>Hour <input type="time" value="08:00" onchange="note('hour '+this.value)"></label>
<label>Tint <input type="color" value="#336699" onchange="note('tint '+this.value)"></label>
<label>Level <input type="range" min="0" max="10" value="4" onchange="note('level '+this.value)"></label></p>
<p id="out">Nothing yet</p>
<script>function note(what){document.getElementById('out').textContent='Changed '+what;}</script>"##;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_date_a_colour_and_a_slider_are_filled_with_a_value_they_can_hold() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-picked-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/picked", get(|| async { Html(PICKED) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/picked")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    // The field as the latest observation shows it. The browser names it after
    // its label, and keeps the space around the label.
    fn field<'a>(observed: &'a Value, name: &str) -> &'a Value {
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| {
                n.get("ref").is_some() && n["name"].as_str().is_some_and(|s| s.trim() == name)
            })
            .unwrap_or_else(|| panic!("{name}: {observed}"))
    }

    // The value is set as a picker would set it, and the page hears of it.
    for (name, text, held, said) in [
        ("Day", "2026-03-04", "2026-03-04", "Changed day 2026-03-04"),
        ("Hour", "09:30", "09:30", "Changed hour 09:30"),
        ("Tint", "#FF8800", "#ff8800", "Changed tint #ff8800"),
        ("Level", "7", "7", "Changed level 7"),
    ] {
        let observed = peer.observe_named(&id, 1, "Booking").await;
        assert_eq!(field(&observed, name)["editable"], true, "{observed}");
        let filled = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","text":text,"ref":field(&observed,name)["ref"]}})).await;
        assert_eq!(
            filled["result"]["input_acknowledged"], true,
            "{name}: {filled}"
        );
        let heard = peer
            .call(
                "browser_wait",
                json!({"session_id":id,"control_epoch":1,"text_contains":said}),
            )
            .await;
        assert_eq!(heard["result"]["condition_observed"], true, "{heard}");
        let observed = peer.observe_named(&id, 1, "Booking").await;
        assert_eq!(field(&observed, name)["value"], held, "{observed}");
    }

    // A value that the field cannot hold is refused, and the field keeps what
    // it held. The page hears nothing.
    for (name, text, held) in [
        ("Day", "next Tuesday", "2026-03-04"),
        ("Hour", "half past nine", "09:30"),
        ("Tint", "orange", "#ff8800"),
        ("Level", "99", "7"),
    ] {
        let observed = peer.observe_named(&id, 1, "Booking").await;
        let refused = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","text":text,"ref":field(&observed,name)["ref"]}})).await;
        assert_eq!(
            refused["error_code"], "browser_invalid_params",
            "{name}: {refused}"
        );
        assert_eq!(refused["execution_state"], "not_sent", "{refused}");
        assert_running(&mut peer, &id).await;
        let observed = peer.observe_named(&id, 1, "Booking").await;
        assert_eq!(field(&observed, name)["value"], held, "{observed}");
        assert!(
            names_of(&observed).contains(&"Changed level 7".to_owned()),
            "{observed}"
        );
    }

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

// A page three screens long. Each screen has a heading, a paragraph and one
// control. The first also has a region and an anchor that leads nowhere. Both
// have a name, and neither is text or a control.
const SHELVES: &str = r##"<!doctype html><meta charset="utf-8"><title>Shelves</title>
<style>body{margin:0} section{height:900px}</style>
<h1>Catalogue</h1>
<section><h2>First shelf</h2><p>Top paragraph</p><button type="button">Top button</button>
<div role="region" aria-label="Offers">Offer text</div> <a aria-label="Bare anchor">Anchor text</a></section>
<section><h2>Second shelf</h2><p>Middle paragraph</p><a href="#middle">Middle link</a></section>
<section><h2>Third shelf</h2><p>Bottom paragraph</p><label>Bottom field <input value="x"></label></section>"##;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_observation_can_be_asked_for_part_of_a_page() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-shelves-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    // Three thousand lines above one button. The page opens scrolled to its end.
    let tall = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Tall</title><h1>Tall</h1>{}
<button type="button">Last button</button><p id="at"></p>
<script>addEventListener('load',()=>{{scrollTo(0,document.documentElement.scrollHeight);document.getElementById('at').textContent='At the end';}});</script>"#,
        (0..3000)
            .map(|i| format!("<p>Line {i:04}</p>"))
            .collect::<String>()
    );
    let app = Router::new()
        .route("/shelves", get(|| async { Html(SHELVES) }))
        .route("/tall", get(move || async move { Html(tall) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/shelves")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    // What a node is, apart from its reference: every observation gives new ones.
    fn shape(node: &Value) -> (String, String, bool) {
        (
            node["role"].as_str().unwrap().to_owned(),
            node["name"].as_str().unwrap().trim().to_owned(),
            node.get("ref").is_some(),
        )
    }
    fn shapes(observed: &Value) -> Vec<(String, String, bool)> {
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(shape)
            .collect()
    }
    let names = |observed: &Value| -> Vec<String> {
        shapes(observed).into_iter().map(|shape| shape.1).collect()
    };

    let whole = peer.observe_named(&id, 1, "Bottom paragraph").await;
    assert!(whole["coverage"].get("filter").is_none(), "{whole}");
    let all = shapes(&whole);

    // Only the controls and the headings: no text, not the region, and not the
    // anchor that leads nowhere. Every control of the whole page is there, in
    // the same order.
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"filter":"interactive"}),
        )
        .await;
    let interactive = &reply["result"];
    assert_eq!(interactive["coverage"]["filter"], "interactive", "{reply}");
    assert_eq!(
        interactive["coverage"]["sequence_complete"], true,
        "{reply}"
    );
    let expected: Vec<_> = all
        .iter()
        .filter(|(role, _, referenced)| *referenced || role == "heading")
        .cloned()
        .collect();
    assert!(all.contains(&("generic".into(), "Bare anchor".into(), false)));
    assert_eq!(shapes(interactive), expected, "{reply}");
    assert_eq!(
        names(interactive),
        [
            "Catalogue",
            "First shelf",
            "Top button",
            "Second shelf",
            "Middle link",
            "Third shelf",
            "Bottom field"
        ],
        "{reply}"
    );

    // A sequence keeps what its first call asked for, and takes no other.
    let first = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"filter":"interactive","max_nodes":3}),
        )
        .await;
    assert_eq!(
        names(&first["result"]),
        ["Catalogue", "First shelf", "Top button"],
        "{first}"
    );
    let cursor = first["result"]["next_cursor"].as_str().unwrap().to_owned();
    let mixed = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"cursor":cursor,"filter":"all"}),
        )
        .await;
    assert_eq!(mixed["error_code"], "browser_invalid_params", "{mixed}");
    assert_eq!(mixed["execution_state"], "not_sent", "{mixed}");
    let rest = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"cursor":cursor}),
        )
        .await;
    assert_eq!(
        names(&rest["result"]),
        ["Second shelf", "Middle link", "Third shelf", "Bottom field"],
        "{rest}"
    );
    assert_eq!(
        rest["result"]["coverage"]["filter"], "interactive",
        "{rest}"
    );
    let unknown = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"filter":"text"}),
        )
        .await;
    assert_eq!(unknown["error_code"], "browser_invalid_params", "{unknown}");
    assert_running(&mut peer, &id).await;

    // Only what shows now: the first screen, with its text and its region.
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"filter":"viewport"}),
        )
        .await;
    let viewport = &reply["result"];
    assert_eq!(viewport["coverage"]["filter"], "viewport", "{reply}");
    assert_eq!(
        names(viewport),
        [
            "Catalogue",
            "First shelf",
            "Top paragraph",
            "Top button",
            "Offers",
            "Offer text",
            "Bare anchor",
            "Anchor text"
        ],
        "{reply}"
    );
    assert!(shapes(viewport).iter().all(|shape| all.contains(shape)));
    // A reference from such a read is used like any other.
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(viewport,"Top button")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");

    // After scrolling one screen down, the second screen shows and the first does not.
    let scrolled = peer
        .call(
            "browser_act",
            json!({"session_id":id,"control_epoch":1,"action":{"kind":"scroll","delta_y":960}}),
        )
        .await;
    assert_eq!(scrolled["result"]["input_acknowledged"], true, "{scrolled}");
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"filter":"viewport"}),
        )
        .await;
    assert_eq!(
        names(&reply["result"]),
        ["Second shelf", "Middle paragraph", "Middle link"],
        "{reply}"
    );

    // What shows can lie far down a long page. One call passes over the six
    // thousand nodes above it, which is more than a whole read looks at in a
    // call, and returns the end of the page.
    let moved = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/tall")}})).await;
    assert!(moved.get("error").is_none(), "{moved}");
    let mut reply = Value::Null;
    for _ in 0..40 {
        reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":1,"filter":"viewport"}),
            )
            .await;
        if names(&reply["result"]).contains(&"At the end".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let end = &reply["result"];
    assert!(names(end).contains(&"Last button".to_owned()), "{reply}");
    assert!(!names(end).contains(&"Line 0000".to_owned()), "{reply}");
    assert!(names(end).len() < 60, "{reply}");
    assert_eq!(end["coverage"]["more"], false, "{reply}");
    assert!(end.get("next_cursor").is_none(), "{reply}");
    assert!(
        end["coverage"]["nodes_scanned"].as_u64().unwrap() > 6000,
        "{reply}"
    );
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"filter":"interactive"}),
        )
        .await;
    assert_eq!(names(&reply["result"]), ["Tall", "Last button"], "{reply}");
    assert_eq!(
        reply["result"]["coverage"]["sequence_complete"], true,
        "{reply}"
    );

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_observation_can_be_asked_for_one_frame() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-frames-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let inner = |name: &'static str| {
        Html(format!(
            r#"<!doctype html><p>Inner {name} text</p><button type="button" onclick="this.textContent='Inner {name} clicked'">Inner {name} button</button>"#
        ))
    };
    let app = Router::new()
        .route("/first", get(move || async move { inner("first") }))
        .route("/second", get(move || async move { inner("second") }))
        .route("/lines/{name}", get(|axum::extract::Path(name): axum::extract::Path<String>| async move {
            Html((1..=15).map(|i| format!("<p>Line {name} {i:02}</p>")).collect::<String>())
        }))
        .route("/lined", get(|headers: axum::http::HeaderMap| async move {
            let host = headers["host"].to_str().unwrap().replace("127.0.0.1", "localhost");
            Html(format!("<!doctype html><h1>Two frames</h1><iframe width=400 height=300 src='/lines/first'></iframe><iframe width=400 height=300 src='http://{host}/lines/second'></iframe>"))
        }))
        .route("/outer", get(|headers: axum::http::HeaderMap| async move {
            // A different host puts the second frame in its own process.
            let host = headers["host"].to_str().unwrap().replace("127.0.0.1", "localhost");
            Html(format!("<!doctype html><h1>Outer</h1><p>Outer text</p><iframe width=400 height=120 src='/first'></iframe><iframe width=400 height=120 src='http://{host}/second'></iframe>"))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/outer")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let names = |observed: &Value| -> Vec<String> {
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().trim().to_owned())
            .collect()
    };

    // A whole observation reads the page and both of its frames, and says of
    // each node which frame holds it.
    let mut whole = Value::Null;
    for _ in 0..40 {
        whole = peer.observe_named(&id, 1, "Outer text").await;
        if ["Inner first button", "Inner second button"]
            .iter()
            .all(|name| names(&whole).contains(&(*name).to_owned()))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let frame_of = |name: &str| {
        node_named(&whole, name)["frame_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (outer, first, second) = (
        frame_of("Outer text"),
        frame_of("Inner first button"),
        frame_of("Inner second button"),
    );
    assert!(
        outer != first && first != second && outer != second,
        "{whole}"
    );

    // One frame: its own content and nothing else, whether or not the frame
    // runs in the page's process.
    for (frame, expected) in [
        (&first, ["Inner first text", "Inner first button"]),
        (&second, ["Inner second text", "Inner second button"]),
        (&outer, ["Outer", "Outer text"]),
    ] {
        let reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":1,"within":frame}),
            )
            .await;
        let read = &reply["result"];
        assert_eq!(names(read), expected, "{reply}");
        assert!(
            read["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| n["frame_id"] == **frame),
            "{reply}"
        );
        assert_eq!(read["coverage"]["within"], **frame, "{reply}");
        assert_eq!(read["coverage"]["sequence_complete"], true, "{reply}");
        assert!(read.get("next_cursor").is_none(), "{reply}");
    }
    // A reference from such a read is used like any other, and a filter narrows
    // the frame further.
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"within":second,"filter":"interactive"}),
        )
        .await;
    assert_eq!(names(&reply["result"]), ["Inner second button"], "{reply}");
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&reply["result"],"Inner second button")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let shown = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Inner second clicked"}),
        )
        .await;
    assert_eq!(shown["result"]["condition_observed"], true, "{shown}");

    // A frame that the page does not hold is not read, and the session stays.
    let missing = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"within":"0123456789ABCDEF0123456789ABCDEF"}),
        )
        .await;
    assert_eq!(
        missing["error_code"], "browser_stale_reference",
        "{missing}"
    );
    assert_eq!(missing["execution_state"], "not_sent", "{missing}");
    assert_running(&mut peer, &id).await;
    // A sequence keeps the frame of its first call, and takes no other.
    let begun = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"within":first,"max_nodes":1}),
        )
        .await;
    assert_eq!(names(&begun["result"]), ["Inner first text"], "{begun}");
    let cursor = begun["result"]["next_cursor"].as_str().unwrap().to_owned();
    let mixed = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"cursor":cursor,"within":second}),
        )
        .await;
    assert_eq!(mixed["error_code"], "browser_invalid_params", "{mixed}");
    let rest = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"cursor":cursor}),
        )
        .await;
    assert_eq!(names(&rest["result"]), ["Inner first button"], "{rest}");
    assert_eq!(rest["result"]["coverage"]["within"], first, "{rest}");

    // A search looks in every frame, and counts what it finds across them:
    // fifteen lines in the first frame, and five of the fifteen in the second.
    let moved = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"navigate","url":format!("{origin}/lined")}})).await;
    assert!(moved.get("error").is_none(), "{moved}");
    let mut found = Value::Null;
    for _ in 0..40 {
        let reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":1,"find":"line"}),
            )
            .await;
        found = reply["result"].clone();
        if found["coverage"]["matches"] == 20 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(found["coverage"]["matches"], 20, "{found}");
    assert_eq!(names(&found).len(), 20, "{found}");
    assert_eq!(names(&found)[14], "Line first 15", "{found}");
    assert_eq!(names(&found)[19], "Line second 05", "{found}");
    assert_eq!(
        found["coverage"]["omitted"],
        json!(["find_match_limit"]),
        "{found}"
    );
    // In one frame, the search finds all fifteen of that frame.
    let second = found["nodes"][19]["frame_id"].clone();
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"find":"line","within":second}),
        )
        .await;
    assert_eq!(reply["result"]["coverage"]["matches"], 15, "{reply}");
    assert_eq!(names(&reply["result"])[14], "Line second 15", "{reply}");
    assert_eq!(
        reply["result"]["coverage"]["sequence_complete"], true,
        "{reply}"
    );

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_search_returns_what_it_finds_with_its_record() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-ledger-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    // A note, a table of 29 customers with an Edit button each, a few fields,
    // a list inside a list, and a list item that holds 40 buttons.
    let ledger = format!(
        r#"<!doctype html><meta charset="utf-8"><title>Ledger</title>
<h1>Customers</h1>
<p>Notes about customer 17 are kept here.</p>
<table><tbody>{}</tbody></table>
<p><label>Contact <input value="carol@example.test"></label> <label>Secret <input type="password" value="hunter2-secret"></label>
<button type="button" aria-label="Archive everything">A</button></p>
<ul><li>Fruits<ul><li>Apple</li><li>Pear</li></ul></li>
<li>Bulk actions {}</li></ul>
<p id="out">Nothing yet</p>
<script>function note(what){{document.getElementById('out').textContent=what;}}</script>"#,
        (17..=45)
            .map(|i| format!("<tr><td>Customer <b>{i}</b></td><td>user{i}@example.test</td><td><button type=\"button\" onclick=\"note('Editing {i}')\">Edit</button></td></tr>"))
            .collect::<String>(),
        (1..=40)
            .map(|i| format!("<button type=\"button\">Act {i:02}</button>"))
            .collect::<String>(),
    );
    let app = Router::new().route("/ledger", get(move || async move { Html(ledger) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable, false);
    let opened = peer
        .call("browser_open", json!({"url":format!("{origin}/ledger")}))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    peer.observe_named(&id, 1, "Nothing yet").await;
    fn names(observed: &Value) -> Vec<String> {
        observed["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap().trim().to_owned())
            .collect()
    }
    async fn find(peer: &mut Peer, id: &str, text: &str) -> Value {
        let reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":1,"find":text}),
            )
            .await;
        assert!(reply.get("error").is_none(), "{text}: {reply}");
        reply["result"].clone()
    }

    // The text is in a note and in a table row, where markup splits it. The
    // row comes whole, with its button. Case and the width of a space do not
    // matter.
    let found = find(&mut peer, &id, "CUSTOMER   17").await;
    assert_eq!(
        names(&found),
        [
            "Notes about customer 17 are kept here.",
            "Customer",
            "17",
            "user17@example.test",
            "Edit"
        ],
        "{found}"
    );
    assert_eq!(found["coverage"]["matches"], 2, "{found}");
    assert_eq!(found["coverage"]["sequence_complete"], true, "{found}");
    assert!(found.get("next_cursor").is_none(), "{found}");
    // The button of that row is clicked without another observation.
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&found,"Edit")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let edited = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Editing 17"}),
        )
        .await;
    assert_eq!(edited["result"]["condition_observed"], true, "{edited}");

    // A control is found by its name, by what it holds, and by its label.
    let found = find(&mut peer, &id, "archive every").await;
    assert_eq!(names(&found), ["Archive everything"], "{found}");
    assert!(found["nodes"][0].get("ref").is_some(), "{found}");
    let found = find(&mut peer, &id, "carol@").await;
    assert_eq!(names(&found), ["Contact"], "{found}");
    assert_eq!(found["nodes"][0]["value"], "carol@example.test", "{found}");
    let found = find(&mut peer, &id, "contact").await;
    assert_eq!(names(&found), ["Contact", "Contact"], "{found}");
    assert_eq!(found["nodes"][0]["role"], "StaticText", "{found}");
    assert!(found["nodes"][1].get("ref").is_some(), "{found}");
    // What a password field holds is never searched.
    let found = find(&mut peer, &id, "hunter2").await;
    assert_eq!(names(&found), Vec::<String>::new(), "{found}");
    assert_eq!(found["coverage"]["matches"], 0, "{found}");
    assert_eq!(found["coverage"]["sequence_complete"], true, "{found}");

    // A record is found without the records inside it, and one inside it
    // without the rest.
    let found = find(&mut peer, &id, "fruits").await;
    assert_eq!(names(&found), ["Fruits"], "{found}");
    let found = find(&mut peer, &id, "apple").await;
    assert_eq!(names(&found), ["Apple"], "{found}");

    // The text is in 31 places: the heading, the note and 29 rows. The first
    // 20 are returned, and the read says that there were more.
    let found = find(&mut peer, &id, "customer").await;
    assert_eq!(found["coverage"]["matches"], 20, "{found}");
    assert_eq!(names(&found).len(), 2 + 18 * 4, "{found}");
    assert_eq!(names(&found)[0], "Customers", "{found}");
    assert_eq!(
        names(&found).last().map(String::as_str),
        Some("Edit"),
        "{found}"
    );
    assert!(names(&found).contains(&"user34@example.test".to_owned()));
    assert!(!names(&found).contains(&"user35@example.test".to_owned()));
    assert_eq!(
        found["coverage"]["omitted"],
        json!(["find_match_limit"]),
        "{found}"
    );
    assert_eq!(found["coverage"]["sequence_complete"], false, "{found}");
    // A search is continued like any other read, and counts across its calls.
    let begun = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"find":"customer","max_nodes":10}),
        )
        .await;
    let mut all = names(&begun["result"]);
    assert_eq!(all.len(), 10, "{begun}");
    let mut cursor = begun["result"]["next_cursor"].as_str().map(str::to_owned);
    let mut last = begun["result"].clone();
    while let Some(token) = cursor {
        let reply = peer
            .call(
                "browser_observe",
                json!({"session_id":id,"control_epoch":1,"cursor":token,"max_nodes":40}),
            )
            .await;
        assert!(reply.get("error").is_none(), "{reply}");
        last = reply["result"].clone();
        all.extend(names(&last));
        cursor = last["next_cursor"].as_str().map(str::to_owned);
    }
    assert_eq!(all, names(&found), "{last}");
    assert_eq!(last["coverage"]["matches"], 20, "{last}");

    // A record with more than 30 nodes is cut there, and the read says so.
    let found = find(&mut peer, &id, "bulk actions").await;
    assert_eq!(names(&found).len(), 30, "{found}");
    assert_eq!(names(&found)[0], "Bulk actions", "{found}");
    assert_eq!(names(&found)[29], "Act 29", "{found}");
    assert_eq!(found["coverage"]["matches"], 1, "{found}");
    assert_eq!(
        found["coverage"]["omitted"],
        json!(["find_record_clipped"]),
        "{found}"
    );

    // With a filter, a search returns the controls of what it finds.
    let reply = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"find":"customer 18","filter":"interactive"}),
        )
        .await;
    assert_eq!(names(&reply["result"]), ["Edit"], "{reply}");
    let clicked = peer.call("browser_act", json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference(&reply["result"],"Edit")}})).await;
    assert_eq!(clicked["result"]["input_acknowledged"], true, "{clicked}");
    let edited = peer
        .call(
            "browser_wait",
            json!({"session_id":id,"control_epoch":1,"text_contains":"Editing 18"}),
        )
        .await;
    assert_eq!(edited["result"]["condition_observed"], true, "{edited}");

    // A search for nothing is refused.
    let empty = peer
        .call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"find":"   "}),
        )
        .await;
    assert_eq!(empty["error_code"], "browser_invalid_params", "{empty}");
    assert_eq!(empty["execution_state"], "not_sent", "{empty}");
    assert_running(&mut peer, &id).await;

    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}
