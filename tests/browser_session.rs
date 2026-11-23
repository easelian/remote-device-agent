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

// The GNU native fixture embeds the production library rather than the CLI.
// Enter its real private supervisor before libtest parses arguments. Rust's
// GNU argv initializer uses priority 00099; this fixture runs immediately after.
// This entry is confined to this test executable and adds no production hook.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[used]
#[unsafe(link_section = ".init_array.00100")]
static SUPERVISOR_ENTRY: extern "C" fn() = {
    extern "C" fn enter() {
        if let Some(result) = rdh_agent::browser::run_internal_supervisor() {
            if let Err(error) = result {
                eprintln!("Browser fixture supervisor: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
    enter
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
        let mut peer = browser_peer(&root, &executable, false, false);
        let opened = peer
            .steps(json!([{"open":url},{"click":{"name":"Record once"}}]))
            .await;
        assert!(opened.get("failed").is_none(), "{opened}");
        fs::write(
            root.join("opened.json"),
            serde_json::to_vec(&opened).unwrap(),
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
        // The browser of the session runs out of a directory named after it.
        let id = opened["session_id"].as_str().unwrap();
        let private = root.join(".rdh-browser");
        let run = below(&private)
            .into_iter()
            .map(|path| private.join(path))
            .find(|path| path.is_dir() && path.file_name().is_some_and(|name| name == id))
            .unwrap();
        if missing_owner {
            // Simulate death after creation but before common owner publication.
            // Only the independent empty-Job witness can retire this directory.
            fs::remove_file(run.join("owner.json")).unwrap();
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        // A new runtime on the same storage waits until the browser the old
        // one left behind is gone, and starts nothing before that.
        let mut peer = browser_peer(&root, &executable, false, false);
        let mut reopened = Value::Null;
        for _ in 0..30 {
            reopened = peer.steps(json!([{"open":url}])).await;
            if reopened.get("failed").is_none() {
                break;
            }
            let (code, state) = failure(&reopened);
            assert!(
                matches!(code, "browser_cleanup_pending" | "browser_profile_busy"),
                "{reopened}"
            );
            assert_eq!(state, "not_sent", "{reopened}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(reopened.get("failed").is_none(), "{reopened}");
        assert_ne!(reopened["session_id"], opened["session_id"]);
        assert!(!run.exists(), "old browser run directory was not retired");
        // The old session is not taken over, and its click is not sent again.
        let sessions = peer.said("browser_read", json!({"what":"sessions"})).await;
        assert_eq!(sessions.lines().count(), 1, "{sessions}");
        assert!(peer.page().await.contains("Record once"));
        assert_eq!(
            received.load(Ordering::Acquire),
            expected,
            "restart replayed website input"
        );
        assert_eq!(peer.does(json!([{"close":true}])).await, "1 close");
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
        let mut message =
            json!({"operation":operation,"params":params,"execution_budget_ms":self.budget});
        if let Some(principal) = principal {
            message["browser_principal"] = principal;
        }
        if let Some(receipt) = receipt {
            message["operation_receipt"] =
                json!({"operation_id":receipt,"instance_id":self.runtime.receipts.instance_id});
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut retries = 0;
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "RPC response deadline"
            );
            self.counter += 1;
            let id = format!("browser-test-{}", self.counter);
            message["id"] = id.clone().into();
            self.runtime
                .receive(&message, &self.sender, &self.ids)
                .unwrap();
            loop {
                let frame = tokio::time::timeout_at(deadline, self.receiver.recv())
                    .await
                    .expect("RPC response deadline")
                    .unwrap();
                let value: Value = serde_json::from_str(frame.message.to_text().unwrap()).unwrap();
                if value["id"] == id {
                    // A reply can arrive before the prior RPC releases its slot.
                    // Only retry an explicit pre-admission refusal, preserving
                    // the operation receipt; accepted or unknown work is never replayed.
                    if retries < 40
                        && value["error_code"] == "operation_busy"
                        && value["execution_state"] == "not_sent"
                        && value.get("execution_ms").is_none()
                    {
                        retries += 1;
                        break;
                    }
                    return value;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    /// A call from the page of the signed-in person who owns the sessions.
    async fn human(&mut self, params: Value) -> Value {
        self.human_as(&"a".repeat(64), params).await
    }
    async fn human_as(&mut self, user: &str, params: Value) -> Value {
        self.raw_as(
            "browser_human",
            params,
            Some(json!({"version":1,"owner":"c".repeat(64),"user":user,"channel":"human"})),
            None,
        )
        .await
    }
    /// A call from the AI, with the receipt a call that changes anything needs.
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        let receipt = rdh_agent::browser::MUTATIONS
            .contains(&operation)
            .then(|| random::hex_token::<16>().unwrap());
        self.raw(operation, params, Some(&"a".repeat(64)), receipt.as_deref())
            .await
    }
    /// The text a call of the AI was answered with.
    async fn said(&mut self, operation: &str, params: Value) -> String {
        let reply = self.call(operation, params).await;
        reply["result"]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{reply}"))
            .to_owned()
    }
    /// Carries out steps. The call itself succeeds; a step that failed is in
    /// the result, under `failed` and in the text.
    async fn steps(&mut self, steps: Value) -> Value {
        self.run(json!({"steps":steps})).await
    }
    async fn run(&mut self, params: Value) -> Value {
        let reply = self.call("browser_do", params).await;
        assert!(reply.get("error").is_none(), "{reply}");
        assert!(reply["result"]["text"].is_string(), "{reply}");
        reply["result"].clone()
    }
    /// Carries out steps that all have to succeed, and returns what was said.
    async fn does(&mut self, steps: Value) -> String {
        let result = self.steps(steps).await;
        assert!(result.get("failed").is_none(), "{result}");
        result["text"].as_str().unwrap().to_owned()
    }
    /// The page the session is on, as the AI reads it.
    async fn page(&mut self) -> String {
        self.said("browser_read", json!({})).await
    }
    /// The files the AI is told about.
    async fn files(&mut self) -> Value {
        let reply = self.call("browser_files", json!({})).await;
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
    /// The one session of the AI is there and in its hands.
    async fn assert_running(&mut self) {
        let sessions = self.said("browser_read", json!({"what":"sessions"})).await;
        assert!(
            sessions.lines().count() == 1 && sessions.ends_with(" ready (yours)"),
            "{sessions}"
        );
    }
}
/// How the step that stopped a `browser_do` call failed.
fn failure(result: &Value) -> (&str, &str) {
    (
        result["failed"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("no step failed: {result}")),
        result["failed"]["state"].as_str().unwrap_or(""),
    )
}
/// The reference a reply gives for the control called `name`.
fn reference(page: &str, name: &str) -> String {
    let found: Vec<_> = page
        .lines()
        .filter(|line| line.ends_with(&format!(": {name}")))
        .filter_map(|line| Some(line.split_once('[')?.1.split_once(']')?.0.to_owned()))
        .collect();
    assert_eq!(found.len(), 1, "{name} in\n{page}");
    found[0].clone()
}
/// Every file and directory below `root`, relative to it.
fn below(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path.clone());
            }
            found.push(path.strip_prefix(root).unwrap().to_owned());
        }
    }
    found.sort();
    found
}

fn browser_peer(root: &std::path::Path, executable: &str, downloads: bool, uploads: bool) -> Peer {
    policy_peer(
        root,
        json!({"enabled":true,"capability_v2":true,"downloads":downloads,"uploads":uploads,"executable":executable}),
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
    let mut peer = browser_peer(&temp.0, &executable, true, false);
    let opened = peer.steps(json!([{"open":url}])).await;
    assert!(opened.get("failed").is_none(), "{opened}");
    let id = opened["session_id"].as_str().unwrap().to_owned();
    // A click that starts a download is not answered with a page that changed.
    let click = |name: &str| json!({"steps":[{"click":{"name":name,"role":"link"}}],"read":"none"});
    let clicked = peer.run(click("Report")).await;
    assert!(clicked.get("failed").is_none(), "{clicked}");
    let mut ready = Value::Null;
    for _ in 0..100 {
        let files = peer.files().await;
        let file = &files["files"][0];
        if file["state"] == "ready" {
            ready = file.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ready["size_bytes"], content.len());
    let artifact = ready["artifact_id"].as_str().unwrap().to_owned();
    assert_eq!(ready["sha256"], rdh_agent::config::sha256_hex(&content));
    // The file belongs to the principal whose session received it: another
    // AI caller is not told of it, and another person cannot read it.
    assert_eq!(
        peer.raw("browser_files", json!({}), Some(&"b".repeat(64)), None)
            .await["result"]["files"],
        json!([])
    );
    let denied = peer
        .human_as(
            &"b".repeat(64),
            json!({"action":"file_read","artifact_id":artifact,"offset":0}),
        )
        .await;
    assert_eq!(denied["error_code"], "browser_file_unavailable");
    let listed = peer
        .human_as(&"b".repeat(64), json!({"action":"files_list"}))
        .await;
    assert_eq!(listed["result"]["files"], json!([]), "{listed}");
    // The AI is told of the file, but only the person's page is given its bytes.
    let refused = peer
        .call(
            "browser_human",
            json!({"action":"file_read","artifact_id":artifact,"offset":0}),
        )
        .await;
    assert_eq!(refused["error_code"], "browser_unauthorized", "{refused}");
    // While the person holds the browser, the AI is told of no file.
    let claim = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"files-view"}))
        .await;
    assert!(claim.get("error").is_none(), "{claim}");
    assert_eq!(peer.files().await["files"], json!([]));
    let listed = peer.human(json!({"action":"files_list"})).await;
    assert_eq!(
        listed["result"]["files"][0]["artifact_id"], artifact,
        "{listed}"
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
    assert_eq!(peer.files().await["files"][0]["artifact_id"], artifact);
    for (name, code) in [
        ("Truncated", "browser_file_incomplete"),
        ("Too large", "browser_file_limit"),
        ("Unknown length", "browser_download_unsupported"),
    ] {
        let clicked = peer.run(click(name)).await;
        assert!(clicked.get("failed").is_none(), "{clicked}");
        let mut failed = false;
        for _ in 0..100 {
            let files = peer.files().await;
            if files["recent_failures"]
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
        assert_eq!(peer.files().await["files"].as_array().unwrap().len(), 1);
    }
    let clicked = peer.run(click("Slow report")).await;
    assert!(clicked.get("failed").is_none(), "{clicked}");
    let mut slow = String::new();
    for _ in 0..100 {
        let files = peer.files().await;
        if let Some(file) = files["files"]
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
    // Only the person stops a download or deletes a file.
    let cancelled = peer
        .human(json!({"action":"file_delete","artifact_id":slow}))
        .await;
    assert_eq!(cancelled["result"]["cancel_requested"], true, "{cancelled}");
    let mut stopped = false;
    for _ in 0..100 {
        if peer.files().await["files"].as_array().unwrap().len() == 1 {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(stopped);
    // Reserve declared sizes before receiving bytes. Three 32-MiB streams plus
    // the completed report fit; a fourth must not create another staged file.
    let old_failures = peer.files().await["recent_failures"]
        .as_array()
        .unwrap()
        .len();
    for expected in 2..=4 {
        let clicked = peer.run(click("Budget hold")).await;
        assert!(clicked.get("failed").is_none(), "{clicked}");
        let mut count = 0;
        for _ in 0..250 {
            count = peer.files().await["files"].as_array().unwrap().len();
            if count == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(count, expected, "{}", peer.files().await);
    }
    let clicked = peer.run(click("Budget hold")).await;
    assert!(clicked.get("failed").is_none(), "{clicked}");
    let mut last = Value::Null;
    for _ in 0..100 {
        last = peer.files().await;
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
        peer.run(json!({"session":id,"steps":[{"close":true}]}))
            .await["text"],
        "1 close"
    );
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    // The finished file outlasts its session and the runtime that received it.
    let mut peer = browser_peer(&temp.0, &executable, true, false);
    assert_eq!(peer.files().await["files"][0]["artifact_id"], artifact);
    let deleted = peer
        .human_as(
            &"b".repeat(64),
            json!({"action":"file_delete","artifact_id":artifact}),
        )
        .await;
    assert_eq!(
        deleted["error_code"], "browser_file_unavailable",
        "{deleted}"
    );
    let deleted = peer
        .human(json!({"action":"file_delete","artifact_id":artifact}))
        .await;
    assert_eq!(deleted["result"]["file_deleted"], true, "{deleted}");
    assert_eq!(peer.files().await["files"], json!([]));
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
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let runtime = peer.runtime.clone();
    assert_eq!(runtime.info()["capabilities"]["browser"]["available"], true);
    assert_eq!(
        runtime.info()["capabilities"]["browser"]["profiles"],
        json!(["temporary"])
    );
    let saved = |found: &dyn Fn(&Value) -> bool| saves.lock().unwrap().iter().any(found);
    let (ai, other) = ("a".repeat(64), "b".repeat(64));
    let open = json!({"steps":[{"open":url}]});

    // A call is carried out for an identity the Server vouches for, and one
    // that changes anything carries a receipt.
    let denied = peer.raw("browser_do", open.clone(), None, None).await;
    assert_eq!(denied["error_code"], "browser_unauthorized");
    let denied = peer.raw("browser_do", open.clone(), Some(&ai), None).await;
    assert_eq!(denied["error_code"], "browser_unauthorized");
    assert_eq!(denied["execution_state"], "not_sent");
    let denied = peer.raw("browser_read", json!({}), None, None).await;
    assert_eq!(denied["error_code"], "browser_unauthorized");
    // An address that is not a web address opens nothing.
    let denied = peer.steps(json!([{"open":"file:///etc/passwd"}])).await;
    assert_eq!(
        failure(&denied),
        ("browser_site_denied", "not_sent"),
        "{denied}"
    );
    assert_eq!(
        peer.said("browser_read", json!({"what":"sessions"})).await,
        "no session is open"
    );

    // A call that is sent twice under one receipt is carried out once, and the
    // receipt keeps the session it led to but not the address.
    let oid = random::hex_token::<16>().unwrap();
    let opened = peer
        .raw("browser_do", open.clone(), Some(&ai), Some(&oid))
        .await;
    assert!(opened.get("error").is_none(), "{opened}");
    assert!(opened["result"].get("failed").is_none(), "{opened}");
    let id = opened["result"]["session_id"].as_str().unwrap().to_owned();
    let repeated = peer.raw("browser_do", open, Some(&ai), Some(&oid)).await;
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
    peer.assert_running().await;

    // Another principal is not told of the session and cannot read, see,
    // drive or close it, whether it names the session or not.
    let sessions = peer
        .raw(
            "browser_read",
            json!({"what":"sessions"}),
            Some(&other),
            None,
        )
        .await;
    assert_eq!(sessions["result"]["text"], "no session is open");
    for (operation, params, code) in [
        ("browser_read", json!({}), "browser_no_session"),
        ("browser_read", json!({"what":"tabs"}), "browser_no_session"),
        ("browser_see", json!({}), "browser_no_session"),
        (
            "browser_read",
            json!({"session":id}),
            "browser_session_unavailable",
        ),
        (
            "browser_see",
            json!({"session":id}),
            "browser_session_unavailable",
        ),
        (
            "browser_do",
            json!({"session":id,"steps":[{"click":{"name":"Save customer"}}]}),
            "browser_session_unavailable",
        ),
        (
            "browser_do",
            json!({"session":id,"steps":[{"close":true}]}),
            "browser_session_unavailable",
        ),
    ] {
        let receipt = (operation == "browser_do").then(|| random::hex_token::<16>().unwrap());
        let denied = peer
            .raw(operation, params, Some(&other), receipt.as_deref())
            .await;
        assert_eq!(denied["error_code"], code, "{operation}: {denied}");
    }
    let foreign = peer
        .raw(
            "browser_do",
            json!({"steps":[{"click":{"name":"Save customer"}}]}),
            Some(&other),
            Some(&random::hex_token::<16>().unwrap()),
        )
        .await;
    assert_eq!(
        foreign["result"]["failed"]["code"], "browser_no_session",
        "{foreign}"
    );
    let listed = peer.human_as(&other, json!({"action":"list"})).await;
    assert_eq!(listed["result"]["sessions"], json!([]), "{listed}");
    let denied = peer
        .human_as(
            &other,
            json!({"action":"claim","session_id":id,"view_id":"test-view"}),
        )
        .await;
    assert_eq!(
        denied["error_code"], "browser_session_unavailable",
        "{denied}"
    );
    peer.assert_running().await;
    assert!(saves.lock().unwrap().is_empty());

    // A second runtime on the same private storage starts nothing while this
    // one lives: it must not clear away what belongs to the running session.
    let mut competing = browser_peer(&temp.0, &executable, false, false);
    let blocked = competing.steps(json!([{"open":url}])).await;
    assert_eq!(
        failure(&blocked),
        ("browser_profile_busy", "not_sent"),
        "{blocked}"
    );
    assert!(competing.runtime.shutdown().await);
    drop(competing);
    peer.assert_running().await;

    // The page is read without what a password field holds.
    let page = peer.page().await;
    assert!(
        page.starts_with(&format!("RDH browser fixture — {url}\n")),
        "{page}"
    );
    assert!(page.contains("Test customer"), "{page}");
    assert!(!page.contains("MUST_NOT_DISCLOSE"), "{page}");
    let screenshot = peer.call("browser_see", json!({})).await;
    assert!(screenshot.get("error").is_none(), "{screenshot}");
    assert_eq!(screenshot["result"]["mime_type"], "image/jpeg");
    assert!(screenshot["result"]["data"].as_str().unwrap().len() < 512 * 1024);
    assert!(screenshot["result"]["width"].as_u64().unwrap() <= 1920);
    assert!(!screenshot.to_string().contains("MUST_NOT_DISCLOSE"));
    let filled = peer
        .does(json!([{"fill":reference(&page, "Name"),"text":"客户 😀"}]))
        .await;
    assert!(!filled.contains("MUST_NOT_DISCLOSE"), "{filled}");
    assert!(peer.page().await.contains("客户 😀"));
    // A step that is sent twice under one receipt reaches the site once.
    let save = json!({"steps":[{"click":{"name":"Save customer"}}]});
    let save_id = random::hex_token::<16>().unwrap();
    let saved_once = peer
        .raw("browser_do", save.clone(), Some(&ai), Some(&save_id))
        .await;
    assert!(saved_once.get("error").is_none(), "{saved_once}");
    assert!(saved_once["result"].get("failed").is_none(), "{saved_once}");
    let duplicate = peer
        .raw("browser_do", save, Some(&ai), Some(&save_id))
        .await;
    assert_eq!(duplicate["execution_state"], "succeeded");
    peer.does(json!([{"wait":{"text":"Saved 客户 😀"}}])).await;
    assert_eq!(*saves.lock().unwrap(), vec![json!({"name":"客户 😀"})]);

    // Frames of the page's own site and of another are read and worked in.
    let frames = peer.does(json!([{"open":format!("{url}frames")}])).await;
    assert!(
        frames.contains("Save local customer") && frames.contains("Save cross customer"),
        "{frames}"
    );
    assert!(!frames.contains("MUST_NOT_DISCLOSE"), "{frames}");
    for kind in ["local", "cross"] {
        peer.does(json!([
            {"fill":{"name":format!("{kind} name")},"text":kind},
            {"click":{"name":format!("Save {kind} customer")}},
        ]))
        .await;
        for _ in 0..30 {
            if saved(&|v| v["name"] == kind) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saved(&|v| v["name"] == kind),
            "frame {kind}: {:?}",
            *saves.lock().unwrap()
        );
    }
    // A long page is read a part at a time, and the reply says there is more.
    let large = peer.does(json!([{"open":format!("{url}large")}])).await;
    // A page of very many paragraphs is read from its beginning.
    assert!(
        large.contains("/large\nheading: Large page\ntext: bounded text\n"),
        "{}",
        large.lines().take(6).collect::<Vec<_>>().join(" | ")
    );
    assert!(large.len() < 96000, "{}", large.len());
    assert!(
        large.lines().last().unwrap().starts_with("more: "),
        "{}",
        large.lines().last().unwrap()
    );
    // A control under something else is not clicked through it.
    peer.does(json!([{"open":format!("{url}overlay")}])).await;
    let covered = peer
        .steps(json!([{"click":{"name":"Save covered customer"}}]))
        .await;
    assert_eq!(
        failure(&covered),
        ("browser_not_actionable", "not_sent"),
        "{covered}"
    );
    assert_eq!(
        saves.lock().unwrap().len(),
        3,
        "an obscured frame must not submit"
    );

    // A paused session is neither read nor driven until it is resumed.
    assert_eq!(peer.does(json!([{"pause":true}])).await, "1 pause");
    let hidden = peer.call("browser_read", json!({})).await;
    assert_eq!(hidden["error_code"], "browser_paused", "{hidden}");
    let hidden = peer.call("browser_see", json!({})).await;
    assert_eq!(hidden["error_code"], "browser_paused", "{hidden}");
    let held = peer.steps(json!([{"open":format!("{url}next")}])).await;
    assert_eq!(failure(&held), ("browser_paused", "not_sent"), "{held}");
    peer.does(json!([{"resume":true}])).await;
    assert!(peer.page().await.contains("Covered frame"));

    // A dialog stops the steps where it opened, and nothing but an answer
    // gets past it.
    let dialog = peer
        .steps(json!([
            {"open":format!("{url}dialog")},
            {"click":{"name":"Open dialog"}},
            {"open":format!("{url}next")},
        ]))
        .await;
    assert!(dialog.get("failed").is_none(), "{dialog}");
    assert_eq!(dialog["steps_done"], 2, "{dialog}");
    let text = dialog["text"].as_str().unwrap();
    assert!(
        text.contains("an alert dialog opened: \"Synthetic dialog\"")
            && text.contains("\nnot run: 3–3"),
        "{text}"
    );
    // Opening another address is no way round the dialog either.
    let around = peer.steps(json!([{"open":format!("{url}human")}])).await;
    assert_eq!(
        failure(&around),
        ("browser_dialog_open", "not_sent"),
        "{around}"
    );
    let blocked = peer.steps(json!([{"click":{"name":"Open dialog"}}])).await;
    assert_eq!(
        failure(&blocked),
        ("browser_dialog_open", "not_sent"),
        "{blocked}"
    );
    let answered = peer
        .does(json!([{"dialog":"dismiss"},{"open":format!("{url}human")}]))
        .await;
    assert!(
        answered.starts_with("1 dialog dismiss (\"Synthetic dialog\")\n2 open "),
        "{answered}"
    );
    assert!(answered.contains("Human login"), "{answered}");

    // A person takes the browser over. Only their page may ask for that, and
    // until they give it back the AI neither reads nor drives nor resumes.
    let tabs = peer.said("browser_read", json!({"what":"tabs"})).await;
    let human_tab = tabs
        .strip_prefix("* ")
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("{tabs}"))
        .to_owned();
    peer.does(json!([{"tab":"new"}])).await;
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
    for operation in ["browser_read", "browser_see"] {
        let hidden = peer.call(operation, json!({})).await;
        assert_eq!(hidden["error_code"], "browser_paused", "{hidden}");
    }
    for step in [
        json!({"resume":true}),
        json!({"open":format!("{url}next")}),
        json!({"tab":human_tab}),
    ] {
        let held = peer.steps(json!([step])).await;
        assert_eq!(failure(&held), ("browser_paused", "not_sent"), "{held}");
    }
    let sessions = peer.said("browser_read", json!({"what":"sessions"})).await;
    assert!(sessions.ends_with(" ready (with a person)"), "{sessions}");
    let status = peer
        .human(json!({"action":"status","session_id":id,"view_id":"test-view"}))
        .await;
    assert_eq!(status["result"]["controller"], "human", "{status}");
    assert_eq!(status["result"]["lease"]["lease_id"], lease["lease_id"]);
    // Another view of the same person does not get the lease.
    let second_view = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"other-view"}))
        .await;
    assert_eq!(
        second_view["error_code"], "browser_human_busy",
        "{second_view}"
    );
    let forged=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":"0".repeat(32),"control_epoch":epoch})).await;
    assert_eq!(
        forged["error_code"], "browser_human_lease_expired",
        "{forged}"
    );
    let tabs_picture=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch})).await;
    assert_eq!(
        tabs_picture["result"]["tabs"].as_array().unwrap().len(),
        2,
        "{tabs_picture}"
    );
    // The person's page names a tab by the whole ID its own picture lists;
    // the short form is what the AI is told.
    let human_tab = tabs_picture["result"]["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tab| tab["tab_id"].as_str())
        .find(|tab| tab.starts_with(&human_tab))
        .unwrap_or_else(|| panic!("{tabs_picture}"))
        .to_owned();
    let switched=peer.human(json!({"action":"input","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"sequence":1,"frame_id":tabs_picture["result"]["frame_id"],"input":{"kind":"tab","tab_id":human_tab}})).await;
    assert!(switched.get("error").is_none(), "{switched}");
    let old_picture=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch})).await;
    assert_eq!(
        old_picture["result"]["text_input_available"], true,
        "{old_picture}"
    );
    // Text is typed into the field the person saw, not into one that took its place.
    saves.lock().unwrap().push(json!({"replaceFocused":true}));
    for _ in 0..40 {
        if saved(&|v| v["focusReplaced"] == true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(saved(&|v| v["focusReplaced"] == true));
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
        if saved(&|v| v["humanSecret"] == "SYNTHETIC-human-秘密") {
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
    // The browser goes back to the AI only when the person agrees to the AI
    // seeing the page as they leave it.
    let unconfirmed=peer.human(json!({"action":"release","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"share_page_with_ai":false})).await;
    assert_eq!(
        unconfirmed["error_code"], "browser_disclosure_required",
        "{unconfirmed}"
    );
    let hidden = peer.call("browser_read", json!({})).await;
    assert_eq!(hidden["error_code"], "browser_paused", "{hidden}");
    let released=peer.human(json!({"action":"release","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch,"share_page_with_ai":true})).await;
    assert!(released.get("error").is_none(), "{released}");
    let late=peer.human(json!({"action":"screenshot","session_id":id,"view_id":"test-view","lease_id":lease["lease_id"],"control_epoch":epoch})).await;
    assert_eq!(late["error_code"], "browser_human_lease_expired", "{late}");
    // What the person typed is in nothing the AI is given afterwards.
    let after = peer.does(json!([{"wait":{"text":"Logged in"}}])).await;
    assert!(after.contains("Logged in"), "{after}");
    assert!(!after.contains("SYNTHETIC-human"), "{after}");
    for what in ["page", "console", "network"] {
        let read = peer.said("browser_read", json!({"what":what})).await;
        assert!(!read.contains("SYNTHETIC-human"), "{read}");
        assert!(!read.contains("/save"), "{read}");
    }

    let select = peer.does(json!([{"open":format!("{url}select")}])).await;
    // A list is read with the values a step can choose, and which it cannot.
    assert!(
        select.contains("<option value=cn-n> 华北 <option disabled value=off> Disabled"),
        "{select}"
    );
    let selected = peer
        .does(json!([
            {"select":{"name":"Region"},"value":"cn-n"},
            {"wait":{"text":"Selected 华北"}},
        ]))
        .await;
    assert!(
        selected.contains(" value=cn-n: ") && selected.contains("Selected 华北"),
        "{selected}"
    );
    assert!(saved(
        &|v| *v == json!({"selection":"cn-n","inputSeen":true})
    ));
    peer.does(json!([{"scroll":600}])).await;
    for _ in 0..40 {
        if saved(&|v| v["scrollObserved"].as_f64().is_some_and(|y| y >= 600.)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saved(&|v| v["scrollObserved"]
        .as_f64()
        .is_some_and(|y| y >= 600.)));

    // A closed session is gone for the AI and for the person, and leaves
    // nothing in the Agent's private storage.
    assert_eq!(peer.does(json!([{"close":true}])).await, "1 close");
    assert_eq!(
        peer.said("browser_read", json!({"what":"sessions"})).await,
        "no session is open"
    );
    let gone = peer.call("browser_read", json!({"session":id})).await;
    assert_eq!(gone["error_code"], "browser_session_unavailable", "{gone}");
    let gone = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"test-view"}))
        .await;
    assert_eq!(gone["error_code"], "browser_session_unavailable", "{gone}");
    assert!(runtime.shutdown().await);
    drop(peer);
    drop(runtime);
    let private = temp.0.join(".rdh-browser");
    let left = below(&private);
    assert!(!left.is_empty());
    for path in &left {
        assert!(private.join(path).is_dir(), "{left:?}");
        assert!(
            !path.to_string_lossy().contains(&id),
            "the session's directory is left: {left:?}"
        );
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
    let app=Router::new().route("/",get(||async{Html(r#"<!doctype html><meta charset="utf-8"><title>Upload fixture</title><label>Upload proof <input id="file" type="file"></label><label>Comment <input id="comment"></label><p id="state">Ready</p><script>file.onchange=async()=>{const f=file.files[0];await fetch('/upload',{method:'POST',headers:{'x-file-name':f.name},body:f});document.querySelector('#state').textContent='Upload received';};</script>"#)}))
        .route("/upload",post(|State(received):State<Uploads>,headers:axum::http::HeaderMap,body:axum::body::Bytes|async move {
            received.lock().unwrap().push((headers["x-file-name"].to_str().unwrap().into(),body.to_vec()));"ok"
        })).with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let url = format!("{origin}/");
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = browser_peer(&temp.0, &executable, false, true);
    let ai = "a".repeat(64);
    let opened = peer.steps(json!([{"open":url}])).await;
    assert!(opened.get("failed").is_none(), "{opened}");
    assert!(
        opened["text"]
            .as_str()
            .unwrap()
            .contains(" type=file: Upload proof"),
        "{opened}"
    );
    let id = opened["session_id"].as_str().unwrap().to_owned();
    let encode = |chunk: &[u8]| base64::engine::general_purpose::STANDARD.encode(chunk);
    let uploads = |listing: &Value| listing["uploads"].as_array().unwrap().clone();

    // The person takes the browser over and hands a file to the tab they see.
    let claim = peer
        .human(json!({"action":"claim","session_id":id,"view_id":"upload-view"}))
        .await;
    assert!(claim.get("error").is_none(), "{claim}");
    let lease = &claim["result"]["lease"];
    let base = json!({"session_id":id,"view_id":"upload-view","lease_id":lease["lease_id"],"control_epoch":lease["control_epoch"]});
    let with = |action: &str, more: Value| {
        let mut params = base.clone();
        params["action"] = action.into();
        params
            .as_object_mut()
            .unwrap()
            .extend(more.as_object().unwrap().clone());
        params
    };
    let tabs = peer.human(with("screenshot", json!({}))).await;
    let tab = tabs["result"]["page"]["tab_id"].clone();
    let bytes: Vec<u8> = (0..=255).cycle().take(150001).collect();
    let sha = rdh_agent::config::sha256_hex(&bytes);
    let mut begin = with(
        "upload_begin",
        json!({"tab_id":tab,"source_origin":"https://wrong.test","name":"upload-proof.bin","size_bytes":bytes.len(),"sha256":sha,"approved":true}),
    );
    assert_eq!(
        peer.human(begin.clone()).await["error_code"],
        "browser_upload_denied"
    );
    begin["source_origin"] = origin.clone().into();
    let started = peer.human(begin).await;
    assert!(started.get("error").is_none(), "{started}");
    let grant = started["result"]["file_grant_id"].clone();
    assert_eq!(started["result"]["offset"], 0);
    assert_eq!(peer.files().await["uploads"], json!([]));
    for (index, chunk) in bytes.chunks(65536).enumerate() {
        let params = with(
            "upload_chunk",
            json!({"file_grant_id":grant,"offset":index*65536,"data":encode(chunk)}),
        );
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
    let finished = peer
        .human(with("upload_finish", json!({"file_grant_id":grant})))
        .await;
    assert_eq!(finished["result"]["state"], "approved", "{finished}");
    // The AI learns of the file only once the person has given the browser back.
    assert_eq!(peer.files().await["uploads"], json!([]));
    let released = peer
        .human(with("release", json!({"share_page_with_ai":true})))
        .await;
    assert!(released.get("error").is_none(), "{released}");
    let listing = peer.files().await;
    assert_eq!(listing["uploads"][0]["state"], "approved", "{listing}");
    assert!(
        !serde_json::to_string(&listing)
            .unwrap()
            .contains("payload/")
    );
    // Another principal is told of no file.
    assert_eq!(
        peer.raw("browser_files", json!({}), Some(&"b".repeat(64)), None)
            .await["result"]["uploads"],
        json!([])
    );

    // A step that does not reach a file field leaves the approval as it was.
    let upload = |name: &str| json!({"steps":[{"upload":{"name":name},"grant":grant}]});
    let missing = peer.run(upload("No such field")).await;
    assert_eq!(
        failure(&missing),
        ("browser_target_not_found", "not_sent"),
        "{missing}"
    );
    let misplaced = peer.run(upload("Comment")).await;
    assert_eq!(
        failure(&misplaced),
        ("browser_not_actionable", "not_sent"),
        "{misplaced}"
    );
    let unknown = peer
        .run(json!({"steps":[{"upload":{"name":"Upload proof"},"grant":"0".repeat(32)}]}))
        .await;
    assert_eq!(
        failure(&unknown),
        ("browser_upload_denied", "not_sent"),
        "{unknown}"
    );
    assert_eq!(peer.files().await["uploads"][0]["state"], "approved");
    assert!(received.lock().unwrap().is_empty());

    // The file reaches the site once, under the name the person approved,
    // however often the call is sent.
    let receipt = random::hex_token::<16>().unwrap();
    let selected = peer
        .raw(
            "browser_do",
            upload("Upload proof"),
            Some(&ai),
            Some(&receipt),
        )
        .await;
    assert!(selected.get("error").is_none(), "{selected}");
    assert!(selected["result"].get("failed").is_none(), "{selected}");
    assert!(
        selected["result"]["text"]
            .as_str()
            .unwrap()
            .starts_with("1 upload "),
        "{selected}"
    );
    let repeated = peer
        .raw(
            "browser_do",
            upload("Upload proof"),
            Some(&ai),
            Some(&receipt),
        )
        .await;
    assert_eq!(repeated["execution_state"], "succeeded", "{repeated}");
    peer.does(json!([{"wait":{"text":"Upload received"}}]))
        .await;
    assert_eq!(
        *received.lock().unwrap(),
        vec![("upload-proof.bin".into(), bytes)]
    );
    // An approval is spent by its one use.
    let reused = peer.run(upload("Upload proof")).await;
    assert_eq!(
        failure(&reused),
        ("browser_upload_denied", "not_sent"),
        "{reused}"
    );
    assert_eq!(received.lock().unwrap().len(), 1);

    // The person can also approve a file while the AI keeps the browser. The
    // approval is for the tab and the site the AI is on.
    let view = "d".repeat(32);
    let approval = |action: &str, more: Value| {
        let mut params = json!({"action":action,"session_id":id,"view_id":view});
        params
            .as_object_mut()
            .unwrap()
            .extend(more.as_object().unwrap().clone());
        params
    };
    let context = approval("file_upload_context", json!({}));
    let denied = peer.call("browser_human", context.clone()).await;
    assert_eq!(denied["error_code"], "browser_unauthorized", "{denied}");
    let denied = peer.human_as(&"b".repeat(64), context.clone()).await;
    assert_eq!(
        denied["error_code"], "browser_session_unavailable",
        "{denied}"
    );
    let context = peer.human(context).await;
    assert!(context.get("error").is_none(), "{context}");
    let context = &context["result"];
    assert_eq!(context["source_origin"], origin, "{context}");
    assert_eq!(context["uses"], 1, "{context}");
    let second: Vec<u8> = b"second approved file".to_vec();
    let mut begin = approval(
        "file_upload_begin",
        json!({"control_epoch":context["control_epoch"],"tab_id":context["tab_id"],
            "source_origin":"https://wrong.test","name":"second.txt","size_bytes":second.len(),
            "sha256":rdh_agent::config::sha256_hex(&second),"approved":true}),
    );
    assert_eq!(
        peer.human(begin.clone()).await["error_code"],
        "browser_upload_denied"
    );
    begin["source_origin"] = origin.clone().into();
    let started = peer.human(begin).await;
    assert!(started.get("error").is_none(), "{started}");
    let second_grant = started["result"]["file_grant_id"].clone();
    let token = started["result"]["approval_token"].clone();
    assert_eq!(started["result"]["available_to_ai"], false, "{started}");
    let held = json!({"control_epoch":context["control_epoch"],"file_grant_id":second_grant,"approval_token":token});
    let mut chunk = approval("file_upload_chunk", held.clone());
    chunk["offset"] = 0.into();
    chunk["data"] = encode(&second).into();
    // The grant alone does not carry the bytes: the token of the approval does.
    let mut forged = chunk.clone();
    forged["approval_token"] = "0".repeat(32).into();
    let refused = peer.human(forged).await;
    assert!(refused.get("error").is_some(), "{refused}");
    let sent = peer.human(chunk).await;
    assert!(sent.get("error").is_none(), "{sent}");
    // Until the person has finished, the AI cannot use the approval.
    assert_eq!(uploads(&peer.files().await).len(), 1);
    peer.does(json!([{"open":url}])).await;
    let early = peer
        .run(json!({"steps":[{"upload":{"name":"Upload proof"},"grant":second_grant}]}))
        .await;
    assert_eq!(
        failure(&early),
        ("browser_upload_denied", "not_sent"),
        "{early}"
    );
    let finished = peer.human(approval("file_upload_finish", held)).await;
    assert_eq!(finished["result"]["state"], "approved", "{finished}");
    assert_eq!(finished["result"]["available_to_ai"], true, "{finished}");
    let listing = peer.files().await;
    assert!(
        uploads(&listing)
            .iter()
            .any(|upload| upload["file_grant_id"] == second_grant && upload["state"] == "approved"),
        "{listing}"
    );
    assert!(!listing.to_string().contains(token.as_str().unwrap()));
    peer.does(json!([
        {"upload":{"name":"Upload proof"},"grant":second_grant},
        {"wait":{"text":"Upload received"}},
    ]))
    .await;
    assert_eq!(
        received.lock().unwrap().last().unwrap(),
        &("second.txt".to_owned(), second)
    );
    assert_eq!(received.lock().unwrap().len(), 2);
    assert_eq!(peer.does(json!([{"close":true}])).await, "1 close");
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

const REJECTIONS: &str = r#"<!doctype html><meta charset="utf-8"><title>Rejection fixture</title>
<h1>Rejections</h1><label>Name <input id="name" value="old"></label><label>Secret <input type="password"></label>
<label>Notes <textarea id="notes">old first line
old second line</textarea></label>
<button type="button">Plain button</button>
<label>Color <select><option value="red">Red</option><option value="green">Green</option></select></label>
<label><input type="checkbox" checked> Agree</label>
<input readonly value="2026-01-01" aria-label="Pick date" onclick="document.getElementById('picked').textContent='Date picker opened'"><p id="picked"></p>
<p id="edited"></p><script>document.addEventListener('input',e=>{const name={name:'Name',notes:'Notes'}[e.target.id];if(name)document.getElementById('edited').textContent='Filled '+name+': '+JSON.stringify(e.target.value)+'; trusted='+e.isTrusted;});</script>"#;

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
        .route("/banner", get(|| async { Html(BANNER) }))
        .route("/reloading", get(|| async { Html("<!doctype html><title>Reloading</title><h1>Reloading</h1><script>setTimeout(()=>location.reload(),20)</script>") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let opened = peer.does(json!([{"open":format!("{origin}/form")}])).await;
    assert!(opened.contains("Plain button"), "{opened}");

    // A step that does not suit its element is refused before any page effect,
    // and the steps after it are not run.
    for (step, code) in [
        (
            json!({"fill":{"name":"Secret"},"text":"x"}),
            "browser_sensitive_input",
        ),
        (
            json!({"fill":{"name":"Plain button"},"text":"x"}),
            "browser_not_actionable",
        ),
        (
            json!({"select":{"name":"Color"},"value":"unobserved"}),
            "browser_invalid_params",
        ),
        (
            json!({"check":{"name":"Plain button"}}),
            "browser_not_actionable",
        ),
    ] {
        let refused = peer
            .steps(json!([step, {"fill":{"name":"Name"},"text":"must not be typed"}]))
            .await;
        assert_eq!(failure(&refused), (code, "not_sent"), "{step}: {refused}");
        assert_eq!(refused["steps_done"], 0, "{refused}");
        assert!(
            refused["text"]
                .as_str()
                .unwrap()
                .ends_with("\nnot run: 2–2"),
            "{refused}"
        );
        peer.assert_running().await;
    }
    let page = peer.page().await;
    assert!(!page.contains("must not be typed"), "{page}");
    assert!(!page.contains("Filled"), "{page}");
    // Later steps run without anything being taken back first.
    let unchanged = peer.does(json!([{"check":{"name":"Agree"}}])).await;
    assert!(unchanged.contains("(no visible change)"), "{unchanged}");
    let filled = peer
        .does(json!([{"fill":{"name":"Name"},"text":"new"}]))
        .await;
    assert!(
        filled.contains("Filled Name: \"new\"; trusted=true"),
        "fill replaces the earlier value: {filled}"
    );
    for (name, text) in [
        ("Name", "\u{5ba2}\u{6237} \u{1f600} e\u{301}"),
        ("Name", ""),
        ("Notes", "\u{7b2c}\u{4e00}\u{884c}\nsecond \u{1f600}"),
        ("Notes", ""),
    ] {
        peer.does(json!([{"fill":{"name":name},"text":text}])).await;
        // The page's own event confirms the complete value and that a native
        // edit occurred. A reading writes a backslash of the page as two.
        let page = peer.page().await;
        let shown = json!(text).to_string().replace('\\', "\\\\");
        assert!(
            page.contains(&format!("Filled {name}: {shown}; trusted=true")),
            "{page}"
        );
    }
    peer.assert_running().await;

    // A read-only field takes a click, but not text.
    let refused = peer
        .steps(json!([{"fill":{"name":"Pick date"},"text":"x"}]))
        .await;
    assert_eq!(
        failure(&refused),
        ("browser_not_actionable", "not_sent"),
        "{refused}"
    );
    let picked = peer.does(json!([{"click":{"name":"Pick date"}}])).await;
    assert!(picked.contains("Date picker opened"), "{picked}");

    // A covered target in the top document is refused before any page effect.
    peer.does(json!([{"open":format!("{origin}/banner")}]))
        .await;
    let covered = peer.steps(json!([{"click":{"name":"Save record"}}])).await;
    assert_eq!(
        failure(&covered),
        ("browser_not_actionable", "not_sent"),
        "{covered}"
    );
    peer.assert_running().await;
    assert!(!peer.page().await.contains("Record saved"));
    let saved = peer
        .does(json!([{"click":{"name":"Accept cookies"}},{"click":{"name":"Save record"}}]))
        .await;
    assert!(saved.contains("Record saved"), "{saved}");

    // Reading a page that keeps reloading can be refused, but it sends no input,
    // so it never reports an uncertain outcome and never pauses the session.
    peer.run(json!({"steps":[{"open":format!("{origin}/reloading")}],"read":"none"}))
        .await;
    let mut outcomes = std::collections::BTreeMap::<String, u32>::new();
    for _ in 0..60 {
        let reply = peer.call("browser_read", json!({})).await;
        assert_ne!(reply["execution_state"], "unknown", "{reply}");
        *outcomes
            .entry(reply["error_code"].as_str().unwrap_or("ok").to_owned())
            .or_default() += 1;
    }
    assert!(outcomes.get("ok").is_some_and(|n| *n > 0), "{outcomes:?}");
    let waited = peer
        .run(json!({"steps":[{"wait":{"text":"Reloading"}}],"read":"none"}))
        .await;
    assert_ne!(waited["failed"]["state"], "unknown", "{waited}");
    peer.assert_running().await;

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
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let opened = peer.does(json!([{"open":format!("{origin}/plain")}])).await;
    assert!(opened.contains("Still here"), "{opened}");

    // The Agent gives up within the call and says so itself, instead of leaving
    // the caller to run into the response deadline. Whether the page is still
    // the one it was is not known, and the reply says that too.
    peer.budget = 4000;
    let failed = peer
        .steps(json!([{"open":unanswered},{"wait":{"text":"never"}}]))
        .await;
    peer.budget = 15000;
    assert_eq!(
        failure(&failed),
        ("browser_navigation_failed", "unknown"),
        "{failed}"
    );
    let text = failed["text"].as_str().unwrap();
    assert!(
        text.contains("it may or may not have taken effect") && text.ends_with("\nnot run: 2–2"),
        "{text}"
    );

    // The session is not left paused and nothing is left pending: the next call
    // works, and the earlier document is intact.
    peer.assert_running().await;
    let page = peer.page().await;
    assert!(
        page.starts_with(&format!("Plain — {origin}/plain\n")) && page.contains("Still here"),
        "{page}"
    );
    let again = peer.does(json!([{"open":format!("{origin}/plain")}])).await;
    assert!(again.contains("Still here"), "{again}");

    drop(silent);
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

/// A link wrapped over two lines is hit where the click lands: on its first
/// fragment, not at the center of a box that also covers other text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_link_wrapped_over_two_lines_is_clicked() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-wrapped-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new()
        .route("/wrapped", get(|| async { Html(WRAPPED) }))
        .route(
            "/landed",
            get(|| async { Html("<!doctype html><h1>Landed</h1>") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let opened = peer
        .does(json!([{"open":format!("{origin}/wrapped")}]))
        .await;
    assert!(opened.contains(": Wrapped link"), "{opened}");
    let clicked = peer
        .steps(json!([{"click":{"name":"Wrapped link"}},{"wait":{"url":"/landed"}}]))
        .await;
    let laid_out = peer
        .said("browser_read", json!({"what":"styles","find":"Wrapped"}))
        .await;
    assert!(clicked.get("failed").is_none(), "{clicked}\n{laid_out}");
    assert!(
        clicked["text"].as_str().unwrap().contains("Landed"),
        "{clicked}"
    );
    peer.assert_running().await;
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

/// A target below the viewport is brought into view and clicked, in the page
/// itself and in a frame of another site.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_target_below_the_viewport_is_scrolled_to_and_clicked() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-below-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new()
        .route("/below", get(|| async { Html(BELOW) }))
        .route("/framed", get(|| async { Html(r#"<!doctype html><button type="button" onclick="this.textContent='Framed clicked'">Framed button</button>"#) }))
        .route("/below-frame", get(|headers: axum::http::HeaderMap| async move {
            // A different host puts the frame in its own process.
            let host = headers["host"].to_str().unwrap().replace("127.0.0.1", "localhost");
            Html(format!("<!doctype html><div style='height:3000px'></div><iframe width=400 height=200 src='http://{host}/framed'></iframe>"))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let mut failures = Vec::new();
    for (page, button, text) in [
        ("below", "Far button", "Clicked: far"),
        ("below-frame", "Framed button", "Framed clicked"),
    ] {
        let clicked = peer
            .steps(json!([
                {"open":format!("{origin}/{page}")},
                {"click":{"name":button}},
                {"wait":{"text":text}},
            ]))
            .await;
        if clicked.get("failed").is_some() || !clicked["text"].as_str().unwrap().contains(text) {
            failures.push(format!("{button}: {}", clicked["text"].as_str().unwrap()));
        }
        peer.assert_running().await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n---\n"));
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
}

/// A button scrolled out of an inner container is brought into view there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn a_target_scrolled_out_of_its_container_is_clicked() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-inner-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/below", get(|| async { Html(BELOW) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let clicked = peer
        .steps(json!([
            {"open":format!("{origin}/below")},
            {"click":{"name":"Inner button"}},
            {"wait":{"text":"Clicked: inner"}},
        ]))
        .await;
    let laid_out = peer
        .said(
            "browser_read",
            json!({"what":"styles","find":"Inner button"}),
        )
        .await;
    assert!(clicked.get("failed").is_none(), "{clicked}\n{laid_out}");
    peer.assert_running().await;
    assert!(peer.runtime.shutdown().await);
    server.abort();
    let _ = server.await;
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
    let mut peer = browser_peer(&temp.0, &executable, false, false);
    let opened = peer.steps(json!([{"open":format!("{origin}/")}])).await;
    assert!(opened.get("failed").is_none(), "{opened}");
    let first_tab = {
        let tabs = peer.said("browser_read", json!({"what":"tabs"})).await;
        tabs.strip_prefix("* ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("{tabs}"))
            .to_owned()
    };
    let home_page = format!("Home — {origin}/\n");

    // An address the caller gives is refused before the browser does anything:
    // nothing is sent, the session keeps running and the tab keeps its page.
    for refused in [
        METADATA,
        "http://2852039166/",
        "http://100.100.100.200/",
        "http://metadata.google.internal/computeMetadata/v1/",
    ] {
        let denied = peer.steps(json!([{"open":refused}])).await;
        assert_eq!(
            failure(&denied),
            ("browser_site_denied", "not_sent"),
            "{denied}"
        );
    }
    peer.assert_running().await;
    let page = peer.page().await;
    assert!(page.starts_with(&home_page), "{page}");
    // No session is started for such an address either.
    let denied = peer
        .run(json!({"session":"new","steps":[{"open":METADATA}]}))
        .await;
    assert_eq!(
        failure(&denied),
        ("browser_site_denied", "not_sent"),
        "{denied}"
    );
    assert!(denied.get("session_id").is_none(), "{denied}");
    peer.assert_running().await;

    // A link leads there. The click itself succeeds; the navigation is refused
    // by the browser on the Agent's word, and the tab says so in place of a page.
    let blocked_page = "169.254.169.254 — chrome-error://chromewebdata/\n";
    let clicked = peer
        .steps(json!([{"click":{"name":"Metadata link"}}]))
        .await;
    assert!(clicked.get("failed").is_none(), "{clicked}");
    let mut page = String::new();
    for _ in 0..60 {
        page = peer.page().await;
        if !page.starts_with(&home_page) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        page.starts_with(blocked_page) && page.contains("ERR_BLOCKED_BY_CLIENT"),
        "{page}"
    );
    assert!(!page.contains("Home"), "{page}");
    peer.assert_running().await;

    // Setting out for a page that may be opened works as before. A frame that
    // points there is refused; the page around it is read as usual.
    let framed = peer
        .does(json!([{"open":format!("{origin}/framed")}]))
        .await;
    assert!(
        framed.contains(&format!("Framed — {origin}/framed\n")) && framed.contains("After frame"),
        "{framed}"
    );
    assert!(
        framed.contains("net::ERR_BLOCKED_BY_CLIENT")
            && framed.ends_with("\ntext: This page has been blocked by Chrome"),
        "{framed}"
    );

    // A link that opens a new tab: the new tab is refused, not the one clicked in.
    peer.does(json!([{"open":format!("{origin}/")}])).await;
    let clicked = peer.steps(json!([{"click":{"name":"Popup link"}}])).await;
    assert!(clicked.get("failed").is_none(), "{clicked}");
    let mut tabs = String::new();
    for _ in 0..60 {
        tabs = peer.said("browser_read", json!({"what":"tabs"})).await;
        if tabs.lines().count() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(tabs.lines().count(), 2, "{tabs}");
    for line in tabs.lines() {
        let (_, tab) = line.split_at(2);
        let (tab, shown) = tab.split_once(' ').unwrap();
        if tab == first_tab {
            assert_eq!(shown, format!("Home — {origin}/"), "{tabs}");
        } else {
            assert_eq!(shown, blocked_page.trim_end(), "{tabs}");
            let read = peer.said("browser_read", json!({"tab":tab})).await;
            assert!(
                read.starts_with(blocked_page) && read.contains("ERR_BLOCKED_BY_CLIENT"),
                "{read}"
            );
        }
    }
    let first = peer.said("browser_read", json!({"tab":first_tab})).await;
    assert!(first.starts_with(&home_page), "{first}");

    // A page the caller may open redirects there. A request has left the browser
    // by then, so the outcome is uncertain, and the reply says so. The step is
    // reported as refused by the site policy, which is what stopped it.
    let denied = peer
        .run(json!({"tab":first_tab,"steps":[{"open":format!("{origin}/redirect")}]}))
        .await;
    assert_eq!(
        failure(&denied),
        ("browser_site_denied", "unknown"),
        "{denied}"
    );
    assert!(
        denied["text"]
            .as_str()
            .unwrap()
            .contains("it may or may not have taken effect"),
        "{denied}"
    );
    // The session is not left paused for it, and the tab shows that the
    // address was refused.
    let page = peer.page().await;
    assert!(
        page.starts_with(blocked_page) && page.contains("ERR_BLOCKED_BY_CLIENT"),
        "{page}"
    );
    peer.assert_running().await;

    // A name that leads to such an address is refused once that is known, before
    // its request is sent. The test says where names lead, in place of the
    // system's resolver.
    peer.runtime
        .browser_resolver(rdh_agent::browser::sites::resolver(
            |host: String| async move {
                (host == "metadata.test").then(|| vec!["169.254.169.254".parse().unwrap()])
            },
        ));
    let denied = peer.steps(json!([{"open":"http://metadata.test/"}])).await;
    assert_eq!(
        failure(&denied),
        ("browser_site_denied", "not_sent"),
        "{denied}"
    );
    peer.assert_running().await;
    let reopened = peer.does(json!([{"open":format!("{origin}/")}])).await;
    assert!(reopened.contains(&home_page), "{reopened}");

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

/// Only a runtime that compares the address a connection really went to can
/// keep an owner's refusal of private networks. This one does not, so with
/// such a policy it offers no session at all instead of a weaker promise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable native host"]
async fn an_owner_who_refuses_private_networks_is_offered_no_sessions() {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::Ordering;
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").unwrap();
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-strict-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let (port, hits, site) = counting_site(IpAddr::V4(Ipv4Addr::LOCALHOST), "Listed page").await;
    let url = format!("http://127.0.0.1:{port}/");
    let mut peer = policy_peer(
        &temp.0,
        json!({"enabled":true,"capability_v2":true,"downloads":true,"uploads":true,"executable":executable,
            "sites":{"private_networks":"deny","private_allow":[format!("127.0.0.1:{port}")]}}),
    );
    // The device says so before anything is asked of it.
    let info = peer.call("device_info", json!({})).await;
    assert!(info.get("error").is_none(), "{info}");
    let browser = &info["result"]["capabilities"]["browser"];
    assert_eq!(browser["available"], false, "{browser}");
    assert_eq!(browser["reason"], "browser_policy_unsupported", "{browser}");
    assert_eq!(browser["sites"]["private_networks"], "deny", "{browser}");
    let operations = info["result"]["capabilities"]["operations"]
        .as_array()
        .unwrap();
    assert!(operations.iter().any(|name| name == "device_info"));
    for operation in rdh_agent::browser::OPERATIONS {
        assert!(
            !operations.iter().any(|name| name == operation),
            "{operation} is offered: {operations:?}"
        );
    }
    let effective = info["result"]["hardening"]["effective_operations"]
        .as_array()
        .unwrap();
    assert!(
        !effective.iter().any(|name| name
            .as_str()
            .is_some_and(|name| name.starts_with("browser_"))),
        "{effective:?}"
    );

    // A call that is made all the same starts no browser and sends nothing,
    // not even to the local site the owner listed.
    let refused = peer.steps(json!([{"open":url}])).await;
    assert_eq!(
        failure(&refused),
        ("browser_backend_unavailable", "not_sent"),
        "{refused}"
    );
    assert!(refused.get("session_id").is_none(), "{refused}");
    for operation in ["browser_read", "browser_see"] {
        let refused = peer.call(operation, json!({})).await;
        assert_eq!(refused["error_code"], "browser_no_session", "{refused}");
    }
    let sessions = peer.human(json!({"action":"list"})).await;
    assert_eq!(sessions["error_code"], "browser_unavailable", "{sessions}");
    let files = peer.call("browser_files", json!({})).await;
    assert_eq!(files["error_code"], "browser_unavailable", "{files}");
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    // Nothing of a browser was put into the Agent's private storage.
    let left = below(&temp.0.join(".rdh-browser"));
    assert!(
        left.iter()
            .all(|path| temp.0.join(".rdh-browser").join(path).is_dir()),
        "{left:?}"
    );
    assert!(
        !left
            .iter()
            .any(|path| path.to_string_lossy().contains("chromium")),
        "{left:?}"
    );
    site.abort();
    let _ = site.await;
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
        json!({"enabled":true,"capability_v2":true,"executable":executable,
            "sites":{"public":"deny","allow":["listed.test","*.listed.test"],"deny":["no.listed.test"]}}),
    );
    // Every name is public as far as the lookup knows.
    peer.runtime
        .browser_resolver(rdh_agent::browser::sites::resolver(|_| async {
            Some(vec!["93.184.216.34".parse().unwrap()])
        }));
    // The list of public sites does not close this device's own.
    let local_page = format!("Local — {origin}/\n");
    let opened = peer.does(json!([{"open":format!("{origin}/")}])).await;
    assert!(opened.contains(&local_page), "{opened}");

    for refused in [
        "https://unlisted.test/",
        "https://notlisted.test/",
        "https://no.listed.test/",
        "https://93.184.216.34/",
    ] {
        let denied = peer.steps(json!([{"open":refused}])).await;
        assert_eq!(
            failure(&denied),
            ("browser_site_denied", "not_sent"),
            "{denied}"
        );
    }
    peer.assert_running().await;
    assert!(peer.page().await.starts_with(&local_page));
    // Nor is a session started for a site that is not listed.
    let denied = peer
        .run(json!({"session":"new","steps":[{"open":"https://unlisted.test/"}]}))
        .await;
    assert_eq!(
        failure(&denied),
        ("browser_site_denied", "not_sent"),
        "{denied}"
    );
    peer.assert_running().await;
    // A link to an unlisted site is refused as well: the tab leaves its page
    // and shows nothing of the site.
    let clicked = peer
        .steps(json!([{"click":{"name":"Unlisted link"}}]))
        .await;
    assert!(clicked.get("failed").is_none(), "{clicked}");
    let mut page = String::new();
    for _ in 0..60 {
        page = peer.page().await;
        if !page.starts_with(&local_page) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        page.starts_with("unlisted.test — chrome-error://chromewebdata/\n")
            && page.contains("ERR_BLOCKED_BY_CLIENT"),
        "{page}"
    );
    // A listed site passes the policy. These names do not exist, so the browser
    // then fails to find them, which is a different failure. The session stays
    // with the AI after it.
    for listed in ["http://listed.test/", "http://docs.listed.test/"] {
        let failed = peer.steps(json!([{"open":listed}])).await;
        assert_eq!(failure(&failed).0, "browser_navigation_failed", "{failed}");
        peer.assert_running().await;
    }
    let reopened = peer.does(json!([{"open":format!("{origin}/")}])).await;
    assert!(reopened.contains(&local_page), "{reopened}");

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
        let mut policy = json!({"version":1,"browser":{"enabled":true,"capability_v2":true,"executable":executable}});
        if let Some(coexistence) = coexistence {
            policy["browser"]["shell_coexistence"] = coexistence.into();
        }
        if sandbox {
            policy["sandbox"] = json!({"mode":"bwrap","network":true,"hide_home":false});
        }
        policy
    };
    let browser = |peer: &Peer| peer.runtime.info()["capabilities"]["browser"].clone();
    let offered = |peer: &Peer| {
        let operations = peer.runtime.info()["capabilities"]["operations"].clone();
        ["browser_do", "browser_read", "browser_see"]
            .iter()
            .filter(|name| operations.as_array().unwrap().iter().any(|o| o == **name))
            .count()
    };
    let open = json!([{"open":format!("{origin}/")}]);

    // Nothing said: a shell and browser sessions go together, as they did before.
    let mut peer = device_peer(&temp.0, policy(None, false), true).unwrap();
    let report = browser(&peer);
    assert_eq!(report["available"], true, "{report}");
    assert_eq!(
        report["shell"],
        json!({"enabled":true,"sandboxed":false,"coexistence":"allow"}),
        "{report}"
    );
    assert_eq!(offered(&peer), 3);
    let opened = peer.does(open.clone()).await;
    assert!(opened.contains("Beside a shell"), "{opened}");
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
        assert_eq!(offered(&peer), 0);
        let refused = peer.steps(open.clone()).await;
        assert_eq!(
            failure(&refused),
            ("browser_backend_unavailable", "not_sent"),
            "{refused}"
        );
        assert!(refused.get("session_id").is_none(), "{refused}");
        let sessions = peer.human(json!({"action":"list"})).await;
        assert_eq!(sessions["error_code"], "browser_unavailable", "{sessions}");
        assert!(peer.runtime.shutdown().await);
        // The same policy on a device without a shell leaves browser sessions alone.
        let peer = device_peer(&temp.0, policy(Some(coexistence), false), false).unwrap();
        let report = browser(&peer);
        assert_eq!(report["available"], true, "{report}");
        assert_eq!(report["shell"]["enabled"], false, "{report}");
        assert_eq!(offered(&peer), 3);
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
            let opened = peer.does(open).await;
            assert!(opened.contains("Beside a shell"), "{opened}");
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
