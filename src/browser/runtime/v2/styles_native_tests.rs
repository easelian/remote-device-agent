use super::*;

async fn settled(f: &Fixture) {
    let until = deadline();
    loop {
        let m = f.metrics();
        if m["native_objects"] == 0
            && m["leases"]["objects"] == 0
            && m["leases"]["reserved_objects"] == 0
            && m["pending"] == 0
        {
            return;
        }
        assert!(
            Instant::now() < until,
            "native CSS resources did not settle: {m}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_styles_pending_cdp() {
    let mut f = Fixture::open("cdp").await;
    let result = std::panic::AssertUnwindSafe(async {
        f.fixture("(()=>{document.body.innerHTML='<div style=\"display:block;height:2400px\">CSS target</div>';return true;})()").await;
        let mut current_id = f.one("CSS target").await;
        let tab = f.port.tab_id().unwrap();
        let baseline_leases = f.metrics()["leases"]["objects"].as_u64().unwrap();
        for mode in ["cancel", "control", "timeout"] {
            let binding = f.registry.lookup(&current_id, &tab, f.page.control().snapshot().0).unwrap();
            let query = Query::parse(&json!({"kind":"read_styles","node_id":current_id,"property":"display"})).unwrap();
            let page = f.page.clone();
            let (reached, release) = match &f.port {
                native::Port::Cdp(p) => p.hold_styles_reply(),
                _ => unreachable!(),
            };
            let mut pending = Some(tokio::spawn(async move {
                page.evidence(&query, Some(binding), deadline()).await
            }));
            tokio::time::timeout(Duration::from_secs(2), reached).await.unwrap().unwrap();
            assert_eq!(f.metrics()["leases"]["objects"].as_u64().unwrap(), baseline_leases + 1);
            let permit = if mode == "cancel" {
                let task = pending.take().unwrap();
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                None
            } else if mode == "control" {
                Some(f.page.claim_human(Instant::now() + Duration::from_millis(100)).await.unwrap())
            } else {
                let error = pending.take().unwrap().await.unwrap().expect_err("timed CSS read succeeded");
                assert!(matches!(error.code, "browser_timeout_unknown" | "browser_deadline"));
                None
            };
            release.send(()).unwrap();
            if let Some(task) = pending {
                let error = task.await.unwrap().expect_err("stale CSS read disclosed a result");
                assert!(matches!(error.code, "browser_control_changed" | "browser_timeout_unknown"), "{error:?}");
            }
            settled(&f).await;
            assert_eq!(f.metrics()["leases"]["objects"].as_u64().unwrap(), baseline_leases);
            f.registry.clear();
            assert!(f.registry.entries.is_empty());
            if let Some(permit) = permit {
                f.page.return_to_ai(&permit, true, deadline()).await.unwrap();
            }
            let fresh_id = f.one("CSS target").await;
            current_id = fresh_id.clone();
            let fresh = Query::parse(&json!({"kind":"read_styles","node_id":fresh_id,"property":"display"})).unwrap();
            let current = f.page.control().snapshot().0;
            let fresh_binding = f.registry.lookup(&fresh_id, &tab, current).unwrap();
            f.page.evidence(&fresh, Some(fresh_binding), deadline()).await.unwrap();
            settled(&f).await;
        }
        println!("cdp: CSS style reads recover after cancellation, control loss, and timeout");
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_styles_rechecks_identity_and_sensitivity_before_delivery() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    const PAGE: &str = r#"<!doctype html><input id="target" style="display:block">
    <script>
    const channel = new WebSocket('ws://' + location.host + '/control');
    channel.onmessage = event => {
        let target = document.querySelector('#target');
        if (event.data === 'replace') {
            const clone = target.cloneNode();
            target.replaceWith(clone);
            target = clone;
        }
        target.type = 'password';
        target.value = 'synthetic-private-value';
        channel.send('changed');
    };
    </script>"#;
    let (accepted, mut sockets) = tokio::sync::mpsc::channel(1);
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async { axum::response::Html(PAGE) }),
        )
        .route(
            "/control",
            axum::routing::get(move |ws: WebSocketUpgrade| {
                let accepted = accepted.clone();
                async move {
                    ws.on_upgrade(move |socket| async move {
                        let _ = accepted.send(socket).await;
                    })
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut f = Fixture::open("cdp").await;
    let result = std::panic::AssertUnwindSafe(async {
        f.port.navigate(&format!("http://{address}/"), deadline()).await.unwrap();
        let mut socket = tokio::time::timeout_at(deadline().into(), sockets.recv()).await.unwrap().unwrap();
        let tab = f.port.tab_id().unwrap();
        for mode in ["same_node", "replace"] {
            f.fixture("(()=>{const n=document.querySelector('#target');n.type='text';n.value='';return true})()").await;
            let found = f.read(json!({"kind":"find_nodes","tag":"input"})).await.unwrap();
            let id = found["nodes"][0]["node_id"].as_str().unwrap();
            let epoch = f.page.control().snapshot().0;
            let binding = f.registry.lookup(id, &tab, epoch).unwrap();
            let query = Query::parse(&json!({"kind":"read_styles","node_id":id,"property":"display"})).unwrap();
            let page = f.page.clone();
            let native::Port::Cdp(port) = &f.port else { unreachable!() };
            let (reached, release) = port.hold_styles_reply();
            let pending = tokio::spawn(async move { page.evidence(&query, Some(binding), deadline()).await });
            tokio::time::timeout_at(deadline().into(), reached).await.unwrap().unwrap();
            // This socket belongs to the test page. Mutation and acknowledgment
            // proceed independently while its CDP reply is held.
            socket.send(Message::Text(mode.into())).await.unwrap();
            let ack = tokio::time::timeout_at(deadline().into(), socket.recv()).await.unwrap().unwrap().unwrap();
            assert!(matches!(ack, Message::Text(text) if text == "changed"));
            release.send(()).unwrap();
            let result = pending.await.unwrap();
            if mode == "replace" {
                assert_eq!(result.unwrap_err().code, "browser_stale_reference");
            } else {
                let [(0, sem::evidence::Raw::ReadStyles { styles, coverage })] = result.as_ref().unwrap().records.as_slice() else {
                    panic!("unexpected evidence after sensitive transition");
                };
                assert!(styles.candidates.is_empty() && !styles.truncated);
                assert!(coverage.omitted.contains(&sem::evidence::Omitted::SensitiveStyleRules));
            }
            let found = f.read(json!({"kind":"find_nodes","tag":"input"})).await.unwrap();
            let fresh_id = &found["nodes"][0]["node_id"];
            let value = f.read(json!({"kind":"read_styles","node_id":fresh_id,"property":"display"})).await.unwrap();
            assert_eq!(value["styles"]["candidates"], json!([]));
            assert!(value["coverage"]["omitted"].as_array().unwrap().iter().any(|item| item == "sensitive_style_rules"));
            assert!(!value.to_string().contains("synthetic-private-value"));
            settled(&f).await;
        }
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    server.abort();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_styles_reparent_and_claim_never_deliver_held_reply() {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    const PAGE: &str = r#"<!doctype html><style>
        #left { pointer-events: auto }
        #right { pointer-events: none }
    </style><div id="left"><span id="target">CSS target</span></div><div id="right"></div>
    <script>
    const channel = new WebSocket('ws://' + location.host + '/control');
    channel.onmessage = event => {
        if (event.data === 'reparent') {
            document.querySelector('#right').append(document.querySelector('#target'));
        }
        channel.send('changed');
    };
    </script>"#;
    let (accepted, mut sockets) = tokio::sync::mpsc::channel(1);
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { axum::response::Html(PAGE) }))
        .route(
            "/control",
            axum::routing::get(move |ws: WebSocketUpgrade| {
                let accepted = accepted.clone();
                async move {
                    ws.on_upgrade(move |socket| async move {
                        let _ = accepted.send(socket).await;
                    })
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut f = Fixture::open("cdp").await;
    let result = std::panic::AssertUnwindSafe(async {
        f.port.navigate(&format!("http://{address}/"), deadline()).await.unwrap();
        let mut socket = tokio::time::timeout_at(deadline().into(), sockets.recv())
            .await.unwrap().unwrap();
        let tab = f.port.tab_id().unwrap();

        // First prove that a held native reply cannot be delivered after the
        // same node is moved under a different CSS parent.
        f.fixture("(()=>{document.querySelector('#left').append(document.querySelector('#target'));return true})()")
            .await;
        let found = f.read(json!({"kind":"find_nodes","tag":"span","text":"CSS target"})).await.unwrap();
        let id = found["nodes"][0]["node_id"].as_str().unwrap().to_owned();
        let epoch = f.page.control().snapshot().0;
        let binding = f.registry.lookup(&id, &tab, epoch).unwrap();
        let query = Query::parse(&json!({"kind":"read_ancestor_styles","node_id":id,"property":"pointer-events"})).unwrap();
        let page = f.page.clone();
        let native::Port::Cdp(port) = &f.port else { unreachable!() };
        let (reached, release) = port.hold_styles_reply();
        let pending = tokio::spawn(async move { page.evidence(&query, Some(binding), deadline()).await });
        tokio::time::timeout_at(deadline().into(), reached).await.unwrap().unwrap();
        socket.send(Message::Text("reparent".into())).await.unwrap();
        let ack = tokio::time::timeout_at(deadline().into(), socket.recv()).await.unwrap().unwrap().unwrap();
        assert!(matches!(ack, Message::Text(text) if text == "changed"));
        release.send(()).unwrap();
        let error = pending.await.unwrap().expect_err("held reply crossed a reparent");
        assert_eq!(error.code, "browser_stale_reference");
        let found = f.read(json!({"kind":"find_nodes","tag":"span","text":"CSS target"})).await.unwrap();
        let fresh_id = found["nodes"][0]["node_id"].as_str().unwrap();
        let fresh = f.read(json!({"kind":"read_ancestor_styles","node_id":fresh_id,"property":"pointer-events"})).await.unwrap();
        assert!(fresh["styles"]["candidates"].as_array().unwrap().iter().any(|row| row["value"] == "none" && row["ancestor_depth"] == 1));
        settled(&f).await;

        // Reset the node, then prove a human claim while the native reply is
        // held also prevents that reply from reaching the AI owner.
        f.fixture("(()=>{const t=document.querySelector('#target');document.querySelector('#left').append(t);return true})()")
            .await;
        let found = f.read(json!({"kind":"find_nodes","tag":"span","text":"CSS target"})).await.unwrap();
        let id = found["nodes"][0]["node_id"].as_str().unwrap().to_owned();
        let epoch = f.page.control().snapshot().0;
        let binding = f.registry.lookup(&id, &tab, epoch).unwrap();
        let query = Query::parse(&json!({"kind":"read_ancestor_styles","node_id":id,"property":"pointer-events"})).unwrap();
        let page = f.page.clone();
        let native::Port::Cdp(port) = &f.port else { unreachable!() };
        let (reached, release) = port.hold_styles_reply();
        let pending = tokio::spawn(async move { page.evidence(&query, Some(binding), deadline()).await });
        tokio::time::timeout_at(deadline().into(), reached).await.unwrap().unwrap();
        let permit = f.page.claim_human(Instant::now() + Duration::from_millis(100)).await.unwrap();
        release.send(()).unwrap();
        let error = pending.await.unwrap().expect_err("held reply crossed human claim");
        assert!(matches!(error.code, "browser_control_changed" | "browser_timeout_unknown"));
        f.page.return_to_ai(&permit, true, deadline()).await.unwrap();
        let found = f.read(json!({"kind":"find_nodes","tag":"span","text":"CSS target"})).await.unwrap();
        let fresh_id = found["nodes"][0]["node_id"].as_str().unwrap();
        let fresh = f.read(json!({"kind":"read_ancestor_styles","node_id":fresh_id,"property":"pointer-events"})).await.unwrap();
        assert!(fresh["styles"]["candidates"].as_array().unwrap().iter().any(|row| row["value"] == "auto" && row["ancestor_depth"] == 1));
        settled(&f).await;
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    server.abort();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
