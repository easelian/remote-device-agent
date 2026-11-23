//! Protocol peers distinguish session retirement from submitted work and cleanup.
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

struct Peer {
    wire: Cdp,
    browser: BufReader<DuplexStream>,
}
impl Peer {
    fn new() -> Self {
        let (client, browser) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(client);
        Self {
            wire: Cdp::new(read, write),
            browser: BufReader::new(browser),
        }
    }
    async fn send(&mut self, value: Value) {
        let mut frame = serde_json::to_vec(&value).unwrap();
        frame.push(0);
        self.browser.get_mut().write_all(&frame).await.unwrap();
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
        assert_eq!(frame.pop(), Some(0));
        serde_json::from_slice(&frame).unwrap()
    }
    async fn barrier(&mut self) {
        let before = self.wire.inner.barriers.load(Ordering::Acquire);
        self.send(json!({"id":0,"result":{}})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.wire.inner.barriers.load(Ordering::Acquire) == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    fn start(
        &self,
        session: &str,
        command: Command,
        params: Value,
    ) -> tokio::task::JoinHandle<Result<Value>> {
        let wire = self.wire.clone();
        let session = session.to_owned();
        tokio::spawn(async move {
            wire.call(
                Some(&session),
                command,
                params,
                (tokio::time::Instant::now() + Duration::from_secs(10)).into_std(),
            )
            .await
        })
    }
    async fn attach(&mut self, session: &str, parent: Option<&str>, target: &str) {
        let mut value = json!({"method":"Target.attachedToTarget","params":{
            "sessionId":session,"targetInfo":{"targetId":target,"type":"page",
                "title":"fixture","url":"about:blank","attached":true},"waitingForDebugger":false}});
        if let Some(parent) = parent {
            value["sessionId"] = parent.into();
        }
        self.send(value).await;
        self.barrier().await;
    }
    async fn detach(&mut self, session: &str) {
        self.send(json!({"method":"Target.detachedFromTarget","params":{"sessionId":session}}))
            .await;
        self.barrier().await;
    }
    async fn refused_input(&mut self, session: &str) {
        let (wire, effect) = self.wire.track_effects();
        let failure = wire
            .call(
                Some(session),
                Command::InputInsertText,
                json!({"text":"not sent"}),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(
            (failure.code, failure.state),
            ("browser_stale_frame", "not_sent")
        );
        assert!(!effect.load(Ordering::Acquire));
        self.quiet().await;
    }
    async fn quiet(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(10), self.browser.read_u8())
                .await
                .is_err()
        );
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.wire.stop();
    }
}

#[tokio::test]
async fn detached_session_stops_at_wire_admission_without_losing_outstanding_ownership() {
    let mut peer = Peer::new();
    // Keep the essential consumer registered but deliberately unread. Admission
    // must react at the reader, rather than waiting for a higher-level consumer.
    let _events = peer.wire.events();
    for session in ["a", "b"] {
        peer.send(json!({"method":"Target.attachedToTarget","params":{
            "sessionId":session,"targetInfo":{"targetId":session,"type":"page",
                "title":"fixture","url":"about:blank","attached":true},"waitingForDebugger":false}})).await;
    }
    peer.barrier().await;
    let mut first = peer.start(
        "a",
        Command::DomResolveNode,
        json!({"backendNodeId":1,"objectGroup":"audit"}),
    );
    let request_a = peer.request().await;
    let second = peer.start(
        "b",
        Command::RuntimeEvaluate(Effect::None),
        json!({"expression":"1","returnByValue":true}),
    );
    let request_b = peer.request().await;
    peer.send(
        json!({"method":"Target.detachedFromTarget","params":{"sessionId":"a","targetId":"a"}}),
    )
    .await;
    peer.barrier().await;
    let detached = tokio::time::timeout(Duration::from_millis(100), &mut first).await;
    let notified = match detached {
        Ok(result) => matches!(
            result.unwrap(),
            Err(Failure {
                state: "unknown",
                ..
            })
        ),
        Err(_) => false,
    };
    let owned_before_late = peer.wire.test_metrics();
    peer.send(json!({"id":request_b["id"],"result":{"result":{"type":"number","value":1}}}))
        .await;
    assert!(second.await.unwrap().is_ok());
    let (wire, effects) = peer.wire.track_effects();
    let later = tokio::spawn(async move {
        wire.call(
            Some("a"),
            Command::InputInsertText,
            json!({"text":"must-not-send"}),
            Instant::now() + Duration::from_millis(200),
        )
        .await
    });
    let mut frame = Vec::new();
    let received = tokio::time::timeout(
        Duration::from_millis(100),
        peer.browser.read_until(0, &mut frame),
    )
    .await;
    let sent_after_detach = matches!(received, Ok(Ok(n)) if n > 0);
    if sent_after_detach {
        assert_eq!(frame.pop(), Some(0));
        let request: Value = serde_json::from_slice(&frame).unwrap();
        peer.send(json!({"id":request["id"],"error":{"code":-32001,"message":"Session with given id not found."}})).await;
    }
    let later_error = later.await.unwrap().unwrap_err();
    // Abort only the caller when it was not notified. The transport must still
    // own its reservation until the explicit late protocol reply arrives.
    if !notified {
        first.abort();
        let _ = first.await;
    }
    peer.send(json!({"id":request_a["id"],"error":{"code":-32001,"message":"Session with given id not found."}})).await;
    peer.barrier().await;
    let value = json!({"retired_caller_notified_unknown":notified,
        "sent_after_detach":sent_after_detach,"effect_marked":effects.load(Ordering::Acquire),
        "later_error":{"code":later_error.code,"state":later_error.state},
        "owned_before_late":owned_before_late,"after_late":peer.wire.test_metrics(),
        "other_session_completed":true});
    if let Ok(path) = std::env::var("RDH_DETACH_EVIDENCE") {
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
    assert!(
        notified && !sent_after_detach && !effects.load(Ordering::Acquire),
        "{value}"
    );
    assert_eq!(later_error.state, "not_sent");
    assert_eq!(owned_before_late["leases"]["reserved_objects"], 3);
    assert_eq!(value["after_late"]["leases"]["reserved_objects"], 0);
}

#[tokio::test]
async fn parent_detach_retires_descendants_without_retiring_another_session_of_the_target() {
    let mut peer = Peer::new();
    let _events = peer.wire.events();
    peer.attach("a", None, "same-target").await;
    peer.attach("child", Some("a"), "child-target").await;
    peer.attach("grandchild", Some("child"), "deep-target")
        .await;
    peer.attach("b", None, "same-target").await;
    let read = peer.start(
        "grandchild",
        Command::DomResolveNode,
        json!({"backendNodeId":1}),
    );
    let request = peer.request().await;
    peer.detach("a").await;
    assert_eq!(read.await.unwrap().unwrap_err().state, "unknown");
    for session in ["a", "child", "grandchild", "never-attached"] {
        peer.refused_input(session).await;
    }
    assert_eq!(peer.wire.test_metrics()["sessions"]["count"], 1);
    let other = peer.start(
        "b",
        Command::RuntimeEvaluate(Effect::None),
        json!({"expression":"42","returnByValue":true}),
    );
    let request_b = peer.request().await;
    peer.send(json!({"id":request_b["id"],"result":{"result":{"value":42}}}))
        .await;
    assert_eq!(other.await.unwrap().unwrap()["result"]["value"], 42);
    peer.send(json!({"id":request["id"],"result":{"object":{"objectId":"late-dead-object"}}}))
        .await;
    peer.barrier().await;
    assert_eq!(peer.wire.test_metrics()["leases"]["objects"], 0);
    assert_eq!(peer.wire.test_metrics()["native_objects"], 0);
    assert_eq!(peer.wire.test_metrics()["leases"]["reserved_objects"], 0);
    for command in [
        Command::RuntimeReleaseObject,
        Command::RuntimeReleaseObjectGroup,
    ] {
        peer.wire
            .cleanup()
            .call(
                Some("grandchild"),
                command,
                json!({"objectId":"late-dead-object","objectGroup":"fixture"}),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
    }
    peer.quiet().await;
    assert!(peer.wire.connected());
}

#[tokio::test]
async fn a_replied_object_is_invalidated_but_a_new_attachment_gets_its_own_admission() {
    let mut peer = Peer::new();
    let _events = peer.wire.events();
    peer.attach("old", None, "target").await;
    let old = peer.start("old", Command::DomResolveNode, json!({"backendNodeId":1}));
    let request = peer.request().await;
    peer.send(json!({"id":request["id"],"result":{"object":{"objectId":"owned"}}}))
        .await;
    assert_eq!(old.await.unwrap().unwrap()["object"]["objectId"], "owned");
    assert_eq!(peer.wire.test_metrics()["leases"]["objects"], 1);
    peer.detach("old").await;
    assert_eq!(peer.wire.test_metrics()["leases"]["objects"], 0);
    peer.attach("new", None, "target").await;
    peer.refused_input("old").await;
    let new = peer.start("new", Command::InputInsertText, json!({"text":"fresh"}));
    let request = peer.request().await;
    assert_eq!(request["sessionId"], "new");
    peer.send(json!({"id":request["id"],"result":{}})).await;
    new.await.unwrap().unwrap();
    assert_eq!(peer.wire.test_metrics()["pending"], 0);
}

#[tokio::test]
async fn detach_between_press_and_release_keeps_unknown_and_the_submitted_input_barrier() {
    let mut peer = Peer::new();
    let _events = peer.wire.events();
    peer.attach("a", None, "target").await;
    let (wire, effect) = peer.wire.track_effects();
    let task = tokio::spawn(async move {
        wire.input_pair(
            Some("a"),
            InputPair::Key,
            json!({"type":"keyDown","key":"Enter"}),
            json!({"type":"keyUp","key":"Enter"}),
            Instant::now() + Duration::from_secs(10),
        )
        .await
    });
    let press = peer.request().await;
    peer.detach("a").await;
    assert_eq!(task.await.unwrap().unwrap_err().state, "unknown");
    assert!(effect.load(Ordering::Acquire));
    assert_eq!(peer.wire.pending(), 1);
    peer.quiet().await;
    peer.send(json!({"id":press["id"],"result":{}})).await;
    peer.barrier().await;
    assert_eq!(peer.wire.pending(), 0);
    assert!(peer.wire.connected());
}

#[tokio::test(start_paused = true)]
async fn unacknowledged_retired_work_escalates_after_a_bounded_local_grace() {
    let mut peer = Peer::new();
    let _events = peer.wire.events();
    peer.attach("a", None, "a").await;
    peer.attach("b", None, "b").await;
    let task = peer.start("a", Command::RuntimeEvaluate(Effect::Page), json!({}));
    peer.request().await;
    peer.detach("a").await;
    assert_eq!(task.await.unwrap().unwrap_err().state, "unknown");
    assert!(peer.wire.connected());
    assert_eq!(peer.wire.pending(), 1);
    let other = peer.start("b", Command::BrowserGetVersion, json!({}));
    let request = peer.request().await;
    peer.send(json!({"id":request["id"],"result":{}})).await;
    other.await.unwrap().unwrap();
    tokio::time::advance(sessions::DETACH_SETTLEMENT + Duration::from_millis(100)).await;
    tokio::task::yield_now().await;
    assert!(!peer.wire.connected());
    assert!(!peer.wire.inner.closed.load(Ordering::Acquire));
    assert_eq!(peer.wire.pending(), 1);
    assert_eq!(peer.wire.test_metrics()["leases"]["reserved_objects"], 2);
    let (wire, effect) = peer.wire.track_effects();
    assert_eq!(
        wire.call(
            Some("b"),
            Command::InputInsertText,
            json!({}),
            (tokio::time::Instant::now() + Duration::from_secs(1)).into_std()
        )
        .await
        .unwrap_err()
        .code,
        "browser_lost"
    );
    assert!(!effect.load(Ordering::Acquire));
    // EOF ends the private transport; its owner still must confirm process close.
    peer.browser.get_mut().shutdown().await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(peer.wire.test_metrics()["pending"], 0);
    assert_eq!(peer.wire.test_metrics()["leases"]["reserved_objects"], 0);
}

#[tokio::test]
async fn a_retired_stream_is_not_reported_as_successfully_closed() {
    for late in [false, true] {
        let mut peer = Peer::new();
        let _events = peer.wire.events();
        peer.attach("a", None, "a").await;
        let open = peer.start(
            "a",
            Command::FetchTakeResponseBodyAsStream,
            json!({"requestId":"download"}),
        );
        let request = peer.request().await;
        if late {
            peer.detach("a").await;
            assert_eq!(open.await.unwrap().unwrap_err().state, "unknown");
            assert_eq!(peer.wire.test_metrics()["leases"]["reserved_streams"], 1);
            peer.send(json!({"id":request["id"],"result":{"stream":"owned-stream"}}))
                .await;
            peer.barrier().await;
        } else {
            peer.send(json!({"id":request["id"],"result":{"stream":"owned-stream"}}))
                .await;
            open.await.unwrap().unwrap();
            peer.detach("a").await;
            let failure = peer
                .wire
                .cleanup()
                .call(
                    Some("a"),
                    Command::IoClose,
                    json!({"handle":"owned-stream"}),
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .unwrap_err();
            assert_eq!(
                (failure.code, failure.state),
                ("browser_cleanup_pending", "unknown")
            );
        }
        assert!(!peer.wire.connected());
        assert_eq!(peer.wire.test_metrics()["leases"]["streams"], 1);
        assert_eq!(peer.wire.test_metrics()["leases"]["reserved_streams"], 0);
        // Neither a request on another session nor a successful no-op is used.
        peer.quiet().await;
    }
}

#[tokio::test]
async fn malformed_duplicate_or_orphan_attachments_fence_before_any_input() {
    let cases = [
        json!({"method":"Target.attachedToTarget","params":{"sessionId":""}}),
        json!({"method":"Target.attachedToTarget","params":{"sessionId":7}}),
        json!({"method":"Target.attachedToTarget","params":{"sessionId":"x".repeat(257)}}),
        json!({"method":"Target.attachedToTarget","params":{"sessionId":"a"}}),
        json!({"method":"Target.attachedToTarget","sessionId":"missing","params":{"sessionId":"child"}}),
        json!({"method":"Target.attachedToTarget","sessionId":true,"params":{"sessionId":"child"}}),
        json!({"method":"Target.attachedToTarget","sessionId":"a","params":{"sessionId":"a"}}),
        json!({"method":"Target.detachedFromTarget","params":{"sessionId":null}}),
    ];
    for event in cases {
        let mut peer = Peer::new();
        let _events = peer.wire.events();
        peer.attach("a", None, "a").await;
        peer.send(event).await;
        peer.barrier().await;
        assert!(!peer.wire.connected());
        let (wire, effect) = peer.wire.track_effects();
        assert_eq!(
            wire.call(
                Some("a"),
                Command::InputInsertText,
                json!({}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap_err()
            .code,
            "browser_lost"
        );
        assert!(!effect.load(Ordering::Acquire));
        peer.quiet().await;
    }
}

#[tokio::test]
async fn retired_ids_with_outstanding_work_and_children_cannot_be_reattached() {
    for child in [false, true] {
        let mut peer = Peer::new();
        let _events = peer.wire.events();
        peer.attach("a", None, "target").await;
        let task = peer.start("a", Command::InputInsertText, json!({}));
        peer.request().await;
        peer.detach("a").await;
        assert_eq!(task.await.unwrap().unwrap_err().state, "unknown");
        if child {
            peer.attach("late-child", Some("a"), "child").await;
        } else {
            peer.attach("a", None, "target").await;
        }
        assert!(!peer.wire.connected());
        assert_eq!(peer.wire.pending(), 1);
    }
}

#[tokio::test]
async fn session_count_and_id_bytes_have_independent_exact_admission_limits() {
    let mut count = Peer::new();
    let mut events = count.wire.events();
    for n in 0..sessions::MAX_SESSIONS {
        count.attach(&format!("session-{n}"), None, "target").await;
        drop(events.recv().await.unwrap());
    }
    assert_eq!(count.wire.test_metrics()["sessions"]["count"], 128);
    count.attach("excess", None, "target").await;
    assert!(!count.wire.connected());
    assert_eq!(count.wire.test_metrics()["sessions"]["count"], 128);

    for extra in [0, 1] {
        let mut bytes = Peer::new();
        let mut events = bytes.wire.events();
        let mut parent = "r".repeat(254);
        bytes.attach(&parent, None, "root").await;
        drop(events.recv().await.unwrap());
        for n in 0..63 {
            let next = format!("{n:03}{}", "x".repeat(253));
            bytes.attach(&next, Some(&parent), "child").await;
            drop(events.recv().await.unwrap());
            parent = next;
        }
        assert_eq!(bytes.wire.test_metrics()["sessions"]["bytes"], 32508);
        bytes
            .attach(&"z".repeat(4 + extra), Some(&parent), "leaf")
            .await;
        if extra == 0 {
            drop(events.recv().await.unwrap());
            assert!(bytes.wire.connected());
            assert_eq!(bytes.wire.test_metrics()["sessions"]["bytes"], 32768);
            bytes.detach(&"r".repeat(254)).await;
            drop(events.recv().await.unwrap());
            assert_eq!(bytes.wire.test_metrics()["sessions"]["bytes"], 0);
            assert_eq!(bytes.wire.test_metrics()["sessions"]["count"], 0);
        } else {
            assert!(!bytes.wire.connected());
            assert_eq!(bytes.wire.test_metrics()["sessions"]["bytes"], 32508);
        }
    }
}

#[tokio::test]
async fn repeated_attachment_lifetimes_do_not_accumulate_retired_ids_or_restore_old_admission() {
    let mut peer = Peer::new();
    let mut events = peer.wire.events();
    for n in 0..1000 {
        let id = format!("session-{n}");
        peer.attach(&id, None, "same-target").await;
        drop(events.recv().await.unwrap());
        peer.detach(&id).await;
        drop(events.recv().await.unwrap());
        assert_eq!(peer.wire.test_metrics()["sessions"]["count"], 0);
        assert_eq!(peer.wire.test_metrics()["sessions"]["bytes"], 0);
    }
    peer.refused_input("session-0").await;
    peer.refused_input("session-999").await;
    assert_eq!(peer.wire.test_metrics()["sessions"]["peak_count"], 1);
    assert_eq!(peer.wire.test_metrics()["events"]["count"], 0);
}

#[tokio::test]
#[ignore = "explicit native Chrome session detach and owned-close regression"]
async fn native_closed_target_never_admits_old_session_input() {
    let root = std::env::temp_dir().join(format!(
        "rdh-detach-{}",
        crate::random::hex_token::<8>().unwrap()
    ));
    crate::storage::create_private_dir(&root).unwrap();
    let browser = std::path::PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let deadline = || Instant::now() + Duration::from_secs(5);
    let mut process = crate::browser::process::BrowserProcess::launch(
        &browser,
        &root,
        "owner",
        None,
        None,
        deadline(),
    )
    .await
    .unwrap();
    let wire = process.engine.backend.wire.clone();
    let mut events = wire.events();
    wire.call(
        None,
        Command::TargetSetAutoAttach,
        json!({"autoAttach":true,"waitForDebuggerOnStart":false,
        "flatten":true,"filter":[{"type":"page"}]}),
        deadline(),
    )
    .await
    .unwrap();
    let first_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["method"] == "Target.attachedToTarget" {
                break event;
            }
        }
    })
    .await
    .unwrap();
    let session_a = first_event["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let target_a = first_event["params"]["targetInfo"]["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(first_event);
    let target_b = wire
        .call(
            None,
            Command::TargetCreateTarget,
            json!({"url":"about:blank"}),
            deadline(),
        )
        .await
        .unwrap()["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let session_b = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["method"] == "Target.attachedToTarget"
                && event["params"]["targetInfo"]["targetId"] == target_b
            {
                break event["params"]["sessionId"].as_str().unwrap().to_owned();
            }
        }
    })
    .await
    .unwrap();
    let pending_wire = wire.clone();
    let pending_session = session_a.clone();
    let mut pending = tokio::spawn(async move {
        pending_wire.call(Some(&pending_session),Command::RuntimeEvaluate(Effect::None),
            json!({"expression":"new Promise(()=>{})","awaitPromise":true,"returnByValue":true}),
            Instant::now()+Duration::from_secs(15)).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!pending.is_finished());
    let retired_at = Instant::now();
    wire.call(
        None,
        Command::TargetCloseTarget,
        json!({"targetId":target_a}),
        deadline(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event["method"] == "Target.detachedFromTarget"
                && event["params"]["sessionId"] == session_a
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_millis(200), &mut pending).await;
    let notification = match result {
        Ok(value) => match value.unwrap() {
            Ok(_) => json!({"completed":"success"}),
            Err(error) => json!({"code":error.code,"state":error.state}),
        },
        Err(_) => {
            pending.abort();
            let _ = pending.await;
            json!({"completed":"still_pending"})
        }
    };
    let after_detach = wire.test_metrics();
    let other = wire
        .call(
            Some(&session_b),
            Command::RuntimeEvaluate(Effect::None),
            json!({"expression":"21*2","returnByValue":true}),
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(other["result"]["value"], 42);
    let (input, effect) = wire.track_effects();
    let before = wire.command_count();
    let error = input
        .call(
            Some(&session_a),
            Command::InputInsertText,
            json!({"text":"must-not-send"}),
            deadline(),
        )
        .await
        .unwrap_err();
    let after_input = wire.test_metrics();
    let attempted = wire.command_count() - before;
    tokio::time::timeout(Duration::from_secs(5), async {
        while wire.connected() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let recovery_ms = retired_at.elapsed().as_secs_f64() * 1000.;
    let after_recovery_fence = wire.test_metrics();
    assert_eq!(after_recovery_fence["pending"], 1);
    assert_eq!(after_recovery_fence["leases"]["reserved_objects"], 2);
    let (other_input, other_effect) = wire.track_effects();
    assert_eq!(
        other_input
            .call(
                Some(&session_b),
                Command::InputInsertText,
                json!({"text":"also fenced during recovery"}),
                deadline()
            )
            .await
            .unwrap_err()
            .code,
        "browser_lost"
    );
    assert!(!other_effect.load(Ordering::Acquire));
    process.close().await.unwrap();
    assert!(!process.profile.exists());
    std::fs::remove_dir_all(root).unwrap();
    let value = json!({"pending_notification":notification,"after_detach":after_detach,
        "input_attempts":attempted,"effect_marked":effect.load(Ordering::Acquire),
        "input_error":{"code":error.code,"state":error.state},"after_input":after_input,
        "recovery_ms":recovery_ms,"after_recovery_fence":after_recovery_fence,
        "other_session_completed":true,"after_close":wire.test_metrics(),"private_directory_removed":true});
    if let Ok(path) = std::env::var("RDH_DETACH_EVIDENCE") {
        std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
    assert!(
        !effect.load(Ordering::Acquire) && error.state == "not_sent",
        "{value}"
    );
    assert_eq!(value["pending_notification"]["state"], "unknown");
}
