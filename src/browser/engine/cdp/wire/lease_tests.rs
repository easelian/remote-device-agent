//! Stateful protocol peers verify admission and ownership independently of the ledger.
use super::*;
use futures_util::FutureExt;
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

struct Peer {
    cdp: Cdp,
    browser: BufReader<DuplexStream>,
}
impl Peer {
    fn new() -> Self {
        let (agent, browser) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(agent);
        let cdp = Cdp::new(read, write);
        cdp.test_sessions(&["s", "a", "b"]);
        Self {
            cdp,
            browser: BufReader::new(browser),
        }
    }
    fn start(
        &self,
        session: &str,
        command: Command,
        params: Value,
    ) -> tokio::task::JoinHandle<Result<Value>> {
        let wire = self.cdp.clone();
        let session = session.to_owned();
        tokio::spawn(async move {
            wire.call(
                Some(&session),
                command,
                params,
                Instant::now() + Duration::from_secs(10),
            )
            .await
        })
    }
    async fn request(&mut self) -> Value {
        let mut frame = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            self.browser.read_until(0, &mut frame),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!frame.is_empty());
        serde_json::from_slice(&frame[..frame.len() - 1]).unwrap()
    }
    async fn send(&mut self, value: Value) {
        let mut frame = serde_json::to_vec(&value).unwrap();
        frame.push(0);
        self.browser.get_mut().write_all(&frame).await.unwrap();
    }
    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"id":request["id"],"result":result})).await;
    }
    async fn call(
        &mut self,
        session: &str,
        command: Command,
        params: Value,
        result: Value,
    ) -> Value {
        let call = self.start(session, command, params);
        let sent = self.request().await;
        assert_eq!(sent["method"], command.spec().method);
        self.reply(&sent, result).await;
        call.await.unwrap().unwrap()
    }
    async fn object(&mut self, session: &str, id: &str, group: &str) {
        self.call(
            session,
            Command::DomResolveNode,
            json!({"backendNodeId":1,"objectGroup":group}),
            json!({"object":{"objectId":id}}),
        )
        .await;
    }
    async fn barrier(&mut self) {
        self.call("s", Command::BrowserGetVersion, json!({}), json!({}))
            .await;
    }
    fn leases(&self) -> Value {
        self.cdp.test_metrics()["leases"].clone()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cdp.stop();
    }
}

#[tokio::test]
async fn exact_object_stream_and_combined_limits_refuse_before_submission() {
    let mut peer = Peer::new();
    for n in 0..256 {
        peer.object("s", &format!("o{n}"), "batch").await;
    }
    for n in 0..16 {
        peer.call(
            "s",
            Command::FetchTakeResponseBodyAsStream,
            json!({"requestId":n}),
            json!({"stream":format!("io{n}")}),
        )
        .await;
    }
    assert_eq!(peer.leases()["objects"], 256);
    assert_eq!(peer.leases()["streams"], 16);
    assert_eq!(peer.leases()["charged_peak"], 272);
    let (wire, effects) = peer.cdp.track_effects();
    for command in [
        Command::RuntimeEvaluate(Effect::Page),
        Command::FetchTakeResponseBodyAsStream,
    ] {
        let error = wire
            .call(
                Some("s"),
                command,
                json!({}),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    }
    assert!(!effects.load(Ordering::Acquire));
    peer.barrier().await;
    peer.call(
        "s",
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"batch"}),
        json!({}),
    )
    .await;
    for n in 0..16 {
        peer.call(
            "s",
            Command::IoClose,
            json!({"handle":format!("io{n}")}),
            json!({}),
        )
        .await;
    }
    assert_eq!(peer.leases()["objects"], 0);
    assert_eq!(peer.leases()["streams"], 0);
    peer.object("s", "again", "batch").await;
    peer.call(
        "s",
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"batch"}),
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn unknown_creators_reserve_capacity_until_real_late_settlement() {
    let mut peer = Peer::new();
    for n in 0..254 {
        peer.object("s", &format!("o{n}"), "held").await;
    }
    let call = peer.start(
        "s",
        Command::RuntimeEvaluate(Effect::None),
        json!({"objectGroup":"late"}),
    );
    let sent = peer.request().await;
    call.abort();
    let _ = call.await;
    assert_eq!(peer.leases()["reserved_objects"], 2);
    let error = peer
        .cdp
        .call(
            Some("s"),
            Command::DomResolveNode,
            json!({}),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    peer.reply(&sent, json!({"result":{"objectId":"late-object"}}))
        .await;
    let release = peer.request().await;
    assert_eq!(release["method"], "Runtime.releaseObject");
    assert_eq!(release["params"]["objectId"], "late-object");
    assert_eq!(peer.leases()["objects"], 255);
    assert_eq!(peer.leases()["reserved_objects"], 0);
    peer.reply(&release, json!({})).await;
    peer.barrier().await;
    assert_eq!(peer.leases()["objects"], 254);
    peer.call(
        "s",
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"held"}),
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn stream_reservations_survive_timeouts_and_errors_do_not_fake_release() {
    let mut peer = Peer::new();
    for n in 0..15 {
        peer.call(
            "s",
            Command::FetchTakeResponseBodyAsStream,
            json!({}),
            json!({"stream":format!("io{n}")}),
        )
        .await;
    }
    let wire = peer.cdp.clone();
    let call = tokio::spawn(async move {
        wire.call(
            Some("s"),
            Command::FetchTakeResponseBodyAsStream,
            json!({}),
            Instant::now() + Duration::from_millis(50),
        )
        .await
    });
    let sent = peer.request().await;
    assert_eq!(
        call.await.unwrap().unwrap_err().code,
        "browser_timeout_unknown"
    );
    assert_eq!(peer.leases()["reserved_streams"], 1);
    let error = peer
        .cdp
        .call(
            Some("s"),
            Command::FetchTakeResponseBodyAsStream,
            json!({}),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "browser_busy");
    peer.reply(&sent, json!({"stream":"late-stream"})).await;
    let release = peer.request().await;
    assert_eq!(release["method"], "IO.close");
    assert_eq!(peer.leases()["streams"], 16);
    peer.reply(&release, json!({})).await;
    peer.barrier().await;
    let call = peer.start("s", Command::IoClose, json!({"handle":"io0"}));
    let close = peer.request().await;
    peer.send(json!({"id":close["id"],"error":{"code":-1,"message":"fixture release failed"}}))
        .await;
    assert!(call.await.unwrap().is_err());
    assert_eq!(peer.leases()["streams"], 15);
    let call = peer.start("s", Command::FetchTakeResponseBodyAsStream, json!({}));
    let open = peer.request().await;
    peer.send(json!({"id":open["id"],"error":{"code":-1}}))
        .await;
    assert!(call.await.unwrap().is_err());
    assert_eq!(peer.leases()["reserved_streams"], 0);
    assert_eq!(peer.leases()["streams"], 15);
}

#[tokio::test]
async fn pending_group_release_preserves_later_members_and_session_scope() {
    let mut peer = Peer::new();
    peer.object("a", "same", "g").await;
    peer.object("b", "same", "g").await;
    let release = peer.start(
        "a",
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"g"}),
    );
    let sent = peer.request().await;
    peer.object("a", "new", "g").await;
    peer.reply(&sent, json!({})).await;
    release.await.unwrap().unwrap();
    assert_eq!(peer.leases()["objects"], 2);
    peer.call(
        "b",
        Command::RuntimeReleaseObject,
        json!({"objectId":"same"}),
        json!({}),
    )
    .await;
    assert_eq!(peer.leases()["objects"], 1);
    peer.call(
        "a",
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"g"}),
        json!({}),
    )
    .await;
    for session in ["a", "b"] {
        peer.call(
            session,
            Command::FetchTakeResponseBodyAsStream,
            json!({}),
            json!({"stream":"same"}),
        )
        .await;
    }
    assert_eq!(peer.leases()["streams"], 2);
    peer.call("a", Command::IoClose, json!({"handle":"same"}), json!({}))
        .await;
    assert_eq!(peer.leases()["streams"], 1);
}

#[tokio::test]
async fn cleanup_has_four_reserved_slots_when_all_ordinary_slots_are_full() {
    let mut peer = Peer::new();
    peer.object("s", "held", "g").await;
    let mut calls = Vec::new();
    let mut ordinary = Vec::new();
    for _ in 0..32 {
        calls.push(peer.start("s", Command::BrowserGetVersion, json!({})));
        ordinary.push(peer.request().await);
    }
    let mut cleanup = Vec::new();
    for _ in 0..4 {
        calls.push(peer.start(
            "s",
            Command::RuntimeReleaseObjectGroup,
            json!({"objectGroup":"g"}),
        ));
        cleanup.push(peer.request().await);
    }
    assert_eq!(peer.cdp.test_metrics()["pending"], 36);
    let error = peer
        .cdp
        .cleanup()
        .call(
            Some("s"),
            Command::RuntimeReleaseObject,
            json!({"objectId":"held"}),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    assert_eq!(peer.leases()["objects"], 1);
    for sent in cleanup {
        peer.reply(&sent, json!({})).await;
    }
    for sent in ordinary {
        peer.reply(&sent, json!({})).await;
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert_eq!(peer.leases()["objects"], 0);
    peer.barrier().await;
}

#[tokio::test]
async fn a_delivered_reply_is_reaped_if_the_receiver_never_consumes_it() {
    let mut peer = Peer::new();
    let wire = peer.cdp.clone();
    let mut call = Box::pin(wire.call(
        Some("s"),
        Command::RuntimeEvaluate(Effect::None),
        json!({}),
        Instant::now() + Duration::from_secs(5),
    ));
    assert!(call.as_mut().now_or_never().is_none());
    let sent = peer.request().await;
    peer.reply(&sent, json!({"result":{"objectId":"unconsumed"}}))
        .await;
    peer.barrier().await;
    assert_eq!(peer.leases()["objects"], 1);
    drop(call);
    let release = peer.request().await;
    assert_eq!(release["params"]["objectId"], "unconsumed");
    peer.reply(&release, json!({})).await;
    peer.barrier().await;
    assert_eq!(peer.leases()["objects"], 0);
}

#[tokio::test]
async fn script_exception_handles_are_released_even_for_a_live_caller() {
    let mut peer = Peer::new();
    let call = peer.start(
        "s",
        Command::RuntimeEvaluate(Effect::None),
        json!({"returnByValue":true}),
    );
    let sent = peer.request().await;
    peer.reply(&sent,json!({"result":{"type":"undefined"},"exceptionDetails":{"exception":{"objectId":"exception"}}})).await;
    assert!(call.await.unwrap().unwrap()["exceptionDetails"].is_object());
    let release = peer.request().await;
    assert_eq!(release["params"]["objectId"], "exception");
    peer.reply(&release, json!({})).await;
    peer.barrier().await;
    assert_eq!(peer.leases()["objects"], 0);
}

#[tokio::test]
async fn failed_or_expired_automatic_cleanup_closes_only_its_owner() {
    for expires in [false, true] {
        let mut peer = Peer::new();
        let mut other = Peer::new();
        let call = peer.start("s", Command::RuntimeEvaluate(Effect::None), json!({}));
        let sent = peer.request().await;
        call.abort();
        let _ = call.await;
        peer.reply(&sent, json!({"result":{"objectId":"orphan"}}))
            .await;
        let cleanup = peer.request().await;
        if !expires {
            peer.send(json!({"id":cleanup["id"],"error":{"code":-1}}))
                .await;
        }
        tokio::time::timeout(Duration::from_secs(4), async {
            while peer.cdp.connected() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(peer.leases()["objects"], 1); // native teardown is not yet proven
        assert_eq!(peer.cdp.test_metrics()["pending"], 0);
        other.barrier().await;
        assert!(other.cdp.connected());
    }
}

#[tokio::test]
async fn unsolicited_runtime_roots_close_the_connection() {
    for event in [
        json!({"method":"Runtime.consoleAPICalled","params":{"args":[{"objectId":"console"}]}}),
        json!({"method":"Runtime.exceptionThrown","params":{"exceptionDetails":{"exception":{"objectId":"error"}}}}),
        json!({"method":"Runtime.inspectRequested","params":{"object":{"objectId":"inspect"}}}),
    ] {
        let mut peer = Peer::new();
        let call = peer.start("s", Command::BrowserGetVersion, json!({}));
        let _ = peer.request().await;
        peer.send(event).await;
        assert!(call.await.unwrap().is_err());
        assert!(!peer.cdp.connected());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit native Chrome lease ceiling, exception and console ownership probe"]
async fn native_object_lease_boundary_and_unsubscribed_runtime_events() {
    use crate::browser::{engine::Event, process::BrowserProcess};
    use std::{fs, path::PathBuf};
    let root = std::env::temp_dir().join(format!(
        "rdh-native-leases-{}",
        crate::random::hex_token::<8>().unwrap()
    ));
    crate::storage::create_private_dir(&root).unwrap();
    let deadline = || Instant::now() + Duration::from_secs(20);
    let executable = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut owner = BrowserProcess::launch(&executable, &root, "profile", None, deadline())
        .await
        .unwrap();
    let engine = owner.engine.clone();
    let wire = engine.backend.wire.clone();
    let mut events = engine.events();
    engine.initialize(deadline()).await.unwrap();
    let context = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Event::Attached { context, .. } = events.recv().await.unwrap().event {
                break context;
            }
        }
    })
    .await
    .unwrap();
    let session = Some(engine.backend.session(&context).unwrap());
    wire.call(
        session,
        Command::RuntimeRunIfWaitingForDebugger,
        json!({}),
        deadline(),
    )
    .await
    .unwrap();
    let document = wire
        .call(
            session,
            Command::RuntimeEvaluate(Effect::None),
            json!({"expression":"document","objectGroup":"probe"}),
            deadline(),
        )
        .await
        .unwrap();
    let described = wire
        .call(
            session,
            Command::DomDescribeNode,
            json!({"objectId":document["result"]["objectId"]}),
            deadline(),
        )
        .await
        .unwrap();
    let backend = described["node"]["backendNodeId"].clone();
    assert!(backend.is_number());
    wire.call(
        session,
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"probe"}),
        deadline(),
    )
    .await
    .unwrap();
    for _ in 0..256 {
        let value = wire
            .call(
                session,
                Command::DomResolveNode,
                json!({"backendNodeId":backend,"objectGroup":"probe"}),
                deadline(),
            )
            .await
            .unwrap();
        assert!(value["object"]["objectId"].is_string());
    }
    let full = wire.test_metrics();
    assert_eq!(full["leases"]["objects"], 256);
    assert_eq!(full["native_objects"], 256);
    let (tracked, effect) = wire.track_effects();
    let error = tracked
        .call(
            session,
            Command::RuntimeEvaluate(Effect::Page),
            json!({"expression":"globalThis.unexpected=1"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    assert!(!effect.load(Ordering::Acquire));
    wire.call(
        session,
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"probe"}),
        deadline(),
    )
    .await
    .unwrap();
    let unchanged = wire
        .call(
            session,
            Command::RuntimeEvaluate(Effect::None),
            json!({"expression":"typeof globalThis.unexpected","returnByValue":true}),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(unchanged["result"]["value"], "undefined");
    let thrown = wire
        .call(
            session,
            Command::RuntimeEvaluate(Effect::None),
            json!({"expression":"throw new Error('owned fixture exception')","returnByValue":true}),
            deadline(),
        )
        .await
        .unwrap();
    assert!(thrown["exceptionDetails"].is_object());
    wire.call(session,Command::RuntimeEvaluate(Effect::None),json!({"expression":"for(let i=0;i<1000;i++)console.log({fixture:i}); 'done'","returnByValue":true}),deadline()).await.unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let metrics = wire.test_metrics();
        if metrics["pending"] == 0 && metrics["native_objects"] == 0 {
            break;
        }
        assert!(
            Instant::now() < until,
            "native cleanup did not settle: {metrics}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let final_metrics = wire.test_metrics();
    assert_eq!(final_metrics["leases"]["objects"], 0);
    assert_eq!(final_metrics["handle_inventory"]["unsolicited_objects"], 0);
    owner.close().await.unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    let evidence = PathBuf::from(std::env::var_os("RDH_D1_EVIDENCE").unwrap())
        .join("native-lease-boundary.json");
    fs::write(evidence,serde_json::to_vec_pretty(&json!({"full":full,"after_cleanup":final_metrics,"overflow_not_sent":true,"exception_reaped":true,"console_calls":1000,"owner_directory_removed":true})).unwrap()).unwrap();
    fs::remove_dir(root).unwrap();
}

#[tokio::test]
async fn confirmed_group_release_prevents_a_second_orphan_release() {
    let mut peer = Peer::new();
    let wire = peer.cdp.clone();
    let mut call = Box::pin(wire.call(
        Some("s"),
        Command::RuntimeEvaluate(Effect::None),
        json!({"objectGroup":"g"}),
        Instant::now() + Duration::from_secs(5),
    ));
    assert!(call.as_mut().now_or_never().is_none());
    let sent = peer.request().await;
    peer.reply(&sent, json!({"result":{"objectId":"orphan"}}))
        .await;
    peer.barrier().await;
    peer.call(
        "s",
        Command::RuntimeReleaseObjectGroup,
        json!({"objectGroup":"g"}),
        json!({}),
    )
    .await;
    drop(call);
    peer.barrier().await;
    assert_eq!(peer.leases()["objects"], 0);
    assert!(peer.cdp.connected());
}

#[tokio::test]
async fn orphan_cleanup_exhaustion_and_unknown_reply_ownership_fail_closed() {
    for unknown_reply in [false, true] {
        let mut peer = Peer::new();
        let call = peer.start("s", Command::RuntimeEvaluate(Effect::None), json!({}));
        let sent = peer.request().await;
        call.abort();
        let _ = call.await;
        let mut cleanup = Vec::new();
        if !unknown_reply {
            for _ in 0..4 {
                cleanup.push(peer.start(
                    "s",
                    Command::RuntimeReleaseObjectGroup,
                    json!({"objectGroup":"g"}),
                ));
                let _ = peer.request().await;
            }
        }
        peer.send(json!({"id":if unknown_reply{json!(9000)}else{sent["id"].clone()},"result":{"result":{"objectId":"orphan"}}})).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while peer.cdp.connected() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for task in cleanup {
            assert!(task.await.unwrap().is_err());
        }
        assert_eq!(peer.cdp.test_metrics()["pending"], 0);
        assert_eq!(peer.leases()["reserved_objects"], 0);
    }
}

#[tokio::test]
async fn ten_thousand_native_lease_replacements_reuse_the_same_budget() {
    let mut peer = Peer::new();
    for batch in 0..50 {
        for n in 0..200 {
            peer.object("s", &format!("{batch}-{n}"), "g").await;
        }
        peer.call(
            "s",
            Command::RuntimeReleaseObjectGroup,
            json!({"objectGroup":"g"}),
            json!({}),
        )
        .await;
        assert_eq!(peer.leases()["objects"], 0);
        assert_eq!(peer.leases()["reserved_objects"], 0);
    }
    assert_eq!(peer.leases()["charged_peak"], 200);
    assert_eq!(
        peer.cdp.test_metrics()["handle_inventory"]["objects_created"],
        10000
    );
}

#[tokio::test]
async fn dropping_all_public_clients_closes_the_pipe_without_explicit_stop() {
    let (client, mut browser) = tokio::io::duplex(1024);
    let (read, write) = tokio::io::split(client);
    let wire = Cdp::new(read, write);
    let second = wire.clone();
    drop(wire);
    drop(second);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), browser.read_u8())
            .await
            .unwrap()
            .is_err()
    );
}
