//! Real Chromium + production RPC/receipt path; explicitly opt in with a browser binary.
#![cfg(target_os = "linux")]
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

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Peer {
    runtime: Arc<Runtime>,
    sender: transport::Sender,
    receiver: tokio::sync::mpsc::Receiver<transport::Frame>,
    ids: Arc<Mutex<HashSet<String>>>,
    counter: u64,
}
impl Peer {
    async fn raw(
        &mut self,
        operation: &str,
        params: Value,
        owner: Option<&str>,
        receipt: Option<&str>,
    ) -> Value {
        self.counter += 1;
        let id = format!("browser-test-{}", self.counter);
        let mut message =
            json!({"id":id,"operation":operation,"params":params,"execution_budget_ms":15000});
        if let Some(owner) = owner {
            message["browser_principal"] = json!({"version":1,"owner":owner});
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
            if reply["result"]["nodes"]
                .as_array()
                .is_some_and(|v| v.iter().any(|n| n["name"] == name))
            {
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
                reply.get("error").is_none() || reply["error_code"] == "browser_input_pending",
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
        .filter(|n| n["name"] == name && n.get("ref").is_some())
        .collect();
    assert_eq!(nodes.len(), 1, "{observation}");
    nodes[0]["ref"].as_str().unwrap().into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable Linux host"]
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
    let config=DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8123","device_id":"browser-test","token":"x".repeat(48),"roots":{"work":temp.0},"allow_write":false,"allow_shell":false})).unwrap();
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
    let (sender, receiver, _) = transport::channel();
    let mut peer = Peer {
        runtime: runtime.clone(),
        sender,
        receiver,
        ids: Arc::new(Mutex::new(HashSet::new())),
        counter: 0,
    };
    let denied = peer.raw("browser_open", json!({}), None, None).await;
    assert_eq!(denied["error_code"], "browser_unauthorized");
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
    assert!(!observation.to_string().contains("MUST_NOT_DISCLOSE"));
    let old_ref = reference(&observation, "Name ");
    let fill=peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","ref":old_ref,"text":"客户 😀"}})).await;
    assert!(fill.get("error").is_none(), "{fill}");
    let stale=peer.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"fill","ref":old_ref,"text":"wrong"}})).await;
    assert_eq!(stale["error_code"], "browser_stale_reference");
    let observation = peer.observe(&id, 1).await;
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
    let close = peer
        .call("browser_control", json!({"session_id":id,"action":"close"}))
        .await;
    assert_eq!(close["result"]["state"], "closed", "{close}");
    assert!(runtime.shutdown().await);
    let private = temp.0.join(".rdh-browser");
    for owner in fs::read_dir(private).unwrap() {
        assert_eq!(fs::read_dir(owner.unwrap().path()).unwrap().count(), 0);
    }
    server.abort();
}
