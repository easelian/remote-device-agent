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
        let mut message =
            json!({"id":id,"operation":operation,"params":params,"execution_budget_ms":15000});
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

fn profile_peer(root: &std::path::Path, executable: &str) -> Peer {
    let config = DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8123","device_id":"profile-test","token":"x".repeat(48),"roots":{"work":root},"allow_write":false,"allow_shell":false})).unwrap();
    let policy = Policy::parse(&json!({"version":1,"browser":{"enabled":true,"persistent_profiles":true,"executable":executable}}), &config).unwrap();
    let runtime =
        Runtime::with_config_path(config, policy, None, None, Some(&root.join("device.json")))
            .unwrap();
    let (sender, receiver, _) = transport::channel();
    Peer {
        runtime,
        sender,
        receiver,
        ids: Arc::new(Mutex::new(HashSet::new())),
        counter: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires RDH_BROWSER_EXECUTABLE and a sandbox-capable Linux host"]
async fn named_profile_persistence_isolation_and_exclusive_ownership() {
    let executable = std::env::var("RDH_BROWSER_EXECUTABLE").expect("set exact test browser path");
    let temp = Temp(std::env::temp_dir().join(format!(
        "rdh-browser-profile-test-{}",
        random::hex_token::<8>().unwrap()
    )));
    rdh_agent::storage::create_private_dir(&temp.0).unwrap();
    let app = Router::new().route("/", get(||async {Html(r#"<!doctype html><meta charset="utf-8"><title>Profile fixture</title><h1 id="state"></h1><button id="remember">Remember</button><script>function show(){document.querySelector('#state').textContent=localStorage.getItem('fixture')==='saved'&&document.cookie.includes('fixture=saved')?'Stored login':'Fresh profile';}remember.onclick=()=>{localStorage.setItem('fixture','saved');document.cookie='fixture=saved; Max-Age=86400; SameSite=Lax';show();};show();</script>"#)}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut peer = profile_peer(&temp.0, &executable);
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
    let mut competing = profile_peer(&temp.0, &executable);
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
    let mut peer = profile_peer(&temp.0, &executable);
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
    let id = fresh["result"]["session_id"].as_str().unwrap();
    peer.observe_named(id, 1, "Fresh profile").await;
    assert!(peer.runtime.shutdown().await);
    drop(peer);
    server.abort();
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
    let resumed = peer
        .call(
            "browser_control",
            json!({"session_id":id,"control_epoch":epoch,"action":"resume"}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "after dialog: {resumed}");
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
        assert_eq!(fs::read_dir(owner.unwrap().path()).unwrap().count(), 0);
    }
    server.abort();
}
