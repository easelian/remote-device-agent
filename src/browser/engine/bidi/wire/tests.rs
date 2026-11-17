use super::*;
use futures_util::FutureExt;
use tokio::io::DuplexStream;
use tokio_tungstenite::tungstenite::protocol::Role;

mod bounds;
mod latency;
mod sequences;
type Browser = WebSocketStream<DuplexStream>;

#[tokio::test]
async fn graceful_fence_retains_late_settlement_and_the_read_side_until_exit() {
    let (channel, mut events, mut browser) = fixture().await;
    let call = channel.submit(Command::CreateTab, deadline()).unwrap();
    let receipt = call.receipt.clone();
    let sent = request(&mut browser).await;
    channel.graceful_stop();
    assert_eq!(request(&mut browser).await["id"], 0);
    assert_eq!(receipt.status(), Settlement::Enqueued);
    send(
        &mut browser,
        json!({"type":"success","id":sent["id"],"result":{"context":"late"}}),
    )
    .await;
    assert_eq!(call.wait().await.unwrap().value["context"], "late");
    send(&mut browser, json!({"type":"success","id":0,"result":{}})).await;
    processed(&mut events, &mut browser).await;
    assert_eq!(receipt.status(), Settlement::Success);
    assert!(!channel.connected());
    assert!(matches!(*channel.lifecycle().borrow(), Mode::Closing));
    browser.close(None).await.unwrap();
    stopped(&channel).await;
    finish(&channel, events).await;
}

#[tokio::test(start_paused = true)]
async fn blocked_writer_has_a_bound_and_never_releases_an_unknown_effect_early() {
    let (channel, events, _browser) = fixture().await;
    let (tracked, effects) = channel.track_effects();
    let call = tracked
        .submit(
            Command::TestPayload {
                effect: Effect::Page,
                text: "x".repeat(200 * 1024),
            },
            deadline(),
        )
        .unwrap();
    let receipt = call.receipt.clone();
    while channel.outgoing.capacity() != SLOTS {
        tokio::task::yield_now().await;
    }
    assert_eq!(channel.pending_effects(), 1);
    assert_eq!(receipt.status(), Settlement::Enqueued);
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(stopped(&channel).await, Fault::WriteTimeout);
    assert_eq!(receipt.status(), Settlement::Unknown(Fault::WriteTimeout));
    assert!(matches!(
        call.wait().await,
        Err(Error::Transport(Fault::WriteTimeout))
    ));
    assert!(effects.load(Ordering::Acquire));
    finish(&channel, events).await;
}

#[tokio::test]
async fn fragmented_message_limit_counts_the_complete_message() {
    use tokio_tungstenite::tungstenite::protocol::frame::{
        Frame,
        coding::{Data, OpCode},
    };
    let (channel, events, mut browser) = fixture().await;
    let writer = tokio::spawn(async move {
        let _ = browser
            .send(Message::Frame(Frame::message(
                vec![b'x'; MAX_MESSAGE / 2],
                OpCode::Data(Data::Text),
                false,
            )))
            .await;
        let _ = browser
            .send(Message::Frame(Frame::message(
                vec![b'x'; MAX_MESSAGE / 2 + 1],
                OpCode::Data(Data::Continue),
                true,
            )))
            .await;
    });
    assert_eq!(stopped(&channel).await, Fault::MessageLimit);
    writer.abort();
    let _ = writer.await;
    finish(&channel, events).await;
}

#[tokio::test]
async fn identical_native_context_ids_cannot_cross_connections() {
    let (first, mut events1, mut browser1) = fixture().await;
    let (second, events2, mut browser2) = fixture().await;
    let context1 = first.context(&json!("same-native-id")).unwrap();
    let context2 = second.context(&json!("same-native-id")).unwrap();
    assert_ne!(context1, context2);
    event(&mut browser1, "same-native-id").await;
    assert!(
        matches!(events1.recv().await.unwrap(),Event::Created{context,..} if context==context1)
    );
    let (tracked, effects) = second.track_effects();
    assert!(matches!(
        tracked.submit(Command::CloseContext(context1), deadline()),
        Err(Error::Rejected(Failure {
            code: "browser_context_scope",
            state: "not_sent"
        }))
    ));
    assert!(!effects.load(Ordering::Acquire));
    assert!(browser2.next().now_or_never().is_none());
    let call = tracked
        .submit(Command::CloseContext(context2), deadline())
        .unwrap();
    let sent = request(&mut browser2).await;
    assert_eq!(sent["params"]["context"], "same-native-id");
    send(
        &mut browser2,
        json!({"type":"success","id":sent["id"],"result":{}}),
    )
    .await;
    call.wait().await.unwrap();
    finish(&first, events1).await;
    finish(&second, events2).await;
}

#[tokio::test(start_paused = true)]
async fn response_at_deadline_settles_once_on_both_sides_of_the_timeout() {
    for before_timeout in [true, false] {
        let (channel, mut events, mut browser) = fixture().await;
        let (reached, release) = channel.hold_next_reply();
        let call = channel.submit(Command::CreateTab, deadline()).unwrap();
        let receipt = call.receipt.clone();
        let request = request(&mut browser).await;
        send(
            &mut browser,
            json!({"type":"success","id":request["id"],"result":{}}),
        )
        .await;
        reached.await.unwrap();
        assert_eq!(receipt.status(), Settlement::Enqueued);
        assert_eq!(channel.pending_effects(), 1);
        if before_timeout {
            release.send(()).unwrap();
            assert!(call.wait().await.is_ok());
        } else {
            tokio::time::advance(Duration::from_secs(31)).await;
            assert!(matches!(call.wait().await, Err(Error::TimeoutUnknown)));
            assert_eq!(receipt.status(), Settlement::Enqueued);
            release.send(()).unwrap();
        }
        processed(&mut events, &mut browser).await;
        assert_eq!(receipt.status(), Settlement::Success);
        assert_eq!(channel.pending_effects(), 0);
        finish(&channel, events).await;
    }
}

async fn fixture() -> (Bidi, Events, Browser) {
    let (agent, browser) = tokio::io::duplex(8192);
    let agent =
        WebSocketStream::from_raw_socket(agent, Role::Client, Some(websocket_config())).await;
    let browser = WebSocketStream::from_raw_socket(browser, Role::Server, None).await;
    let (channel, events) = Bidi::new(agent);
    (channel, events, browser)
}
fn deadline() -> Instant {
    (tokio::time::Instant::now() + Duration::from_secs(30)).into_std()
}
async fn request(browser: &mut Browser) -> Value {
    let message = browser.next().await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
async fn send(browser: &mut Browser, value: Value) {
    browser
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
async fn event(browser: &mut Browser, id: &str) {
    send(
        browser,
        json!({"type":"event","method":CORE_EVENTS[0],"params":{"context":id,"parent":null}}),
    )
    .await;
}
async fn processed(events: &mut Events, browser: &mut Browser) {
    event(browser, "barrier").await;
    let received = events.recv().await.unwrap();
    assert!(matches!(received,Event::Created{context,..} if context.as_str()=="barrier"));
}
async fn stopped(channel: &Bidi) -> Fault {
    let mut state = channel.lifecycle();
    loop {
        if let Mode::Stopped(fault) = *state.borrow_and_update() {
            return fault;
        }
        state.changed().await.unwrap();
    }
}
async fn finish(channel: &Bidi, events: Events) {
    channel.stop();
    drop(events);
    channel.wait_stopped(deadline()).await.unwrap();
    let snapshot = channel.snapshot();
    assert_eq!(snapshot["pending"], 0);
    assert_eq!(snapshot["event_bytes"], 0);
    assert_eq!(snapshot["workers"], 0);
    assert_eq!(snapshot["free_slots"], SLOTS);
}

#[tokio::test(start_paused = true)]
async fn timeout_and_cancel_keep_slots_barriers_and_original_receipts() {
    let (channel, mut events, mut browser) = fixture().await;
    let control = Arc::new(Control::new());
    let (client, effects) = channel
        .authorized(control.clone(), 1, false)
        .track_effects();
    let first = client.submit(Command::CreateTab, deadline()).unwrap();
    let receipt = first.receipt.clone();
    let sent = request(&mut browser).await;
    assert_eq!(sent["method"], "browsingContext.create");
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(matches!(first.wait().await, Err(Error::TimeoutUnknown)));
    assert_eq!(receipt.status(), Settlement::Enqueued);
    assert_eq!(channel.pending_effects(), 1);
    assert_eq!(channel.snapshot()["free_slots"], 31);
    assert!(effects.load(Ordering::Acquire));
    control.change(true);
    assert!(matches!(
        client.submit(Command::CreateTab, deadline()),
        Err(Error::Rejected(Failure {
            code: "browser_control_changed",
            state: "not_sent"
        }))
    ));
    send(
        &mut browser,
        json!({"type":"success","id":sent["id"],"result":{"context":"late"}}),
    )
    .await;
    processed(&mut events, &mut browser).await;
    assert_eq!(receipt.status(), Settlement::Success);
    assert_eq!(channel.pending_effects(), 0);
    assert_eq!(channel.snapshot()["late_success"], 1);
    let read = channel.submit(Command::Tree, deadline()).unwrap();
    let receipt = read.receipt.clone();
    let sent = request(&mut browser).await;
    drop(read);
    assert_eq!(channel.snapshot()["pending"], 1);
    assert_eq!(channel.pending_effects(), 0);
    send(
        &mut browser,
        json!({"type":"error","id":sent["id"],"error":"no such frame","message":"synthetic"}),
    )
    .await;
    processed(&mut events, &mut browser).await;
    assert_eq!(
        receipt.status(),
        Settlement::ProtocolError(ProtocolError::NoSuchFrame)
    );
    assert_eq!(channel.snapshot()["late_error"], 1);
    assert!(effects.load(Ordering::Acquire));
    finish(&channel, events).await;
}

#[tokio::test]
async fn revocation_blocks_delivery_without_erasing_native_settlement() {
    let (channel, mut events, mut browser) = fixture().await;
    let control = Arc::new(Control::new());
    let client = channel.authorized(control.clone(), 1, false);
    let call = client.submit(Command::Tree, deadline()).unwrap();
    let receipt = call.receipt.clone();
    let sent = request(&mut browser).await;
    control.change(true);
    send(
        &mut browser,
        json!({"type":"success","id":sent["id"],"result":{"synthetic_secret":"must-not-deliver"}}),
    )
    .await;
    assert!(matches!(call.wait().await, Err(Error::DeliveryRevoked)));
    assert_eq!(receipt.status(), Settlement::Success);
    processed(&mut events, &mut browser).await;
    finish(&channel, events).await;
}

#[tokio::test]
async fn out_of_order_duplicate_and_unknown_replies_never_answer_new_commands() {
    let (channel, mut events, mut browser) = fixture().await;
    let first = channel.submit(Command::Status, deadline()).unwrap();
    let a = request(&mut browser).await;
    let second = channel.submit(Command::Tree, deadline()).unwrap();
    let b = request(&mut browser).await;
    send(
        &mut browser,
        json!({"type":"success","id":b["id"],"result":{"which":"second"}}),
    )
    .await;
    assert_eq!(second.wait().await.unwrap().value["which"], "second");
    send(
        &mut browser,
        json!({"type":"success","id":b["id"],"result":{"wrong":"duplicate"}}),
    )
    .await;
    send(
        &mut browser,
        json!({"type":"success","id":3,"result":{"wrong":"future"}}),
    )
    .await;
    processed(&mut events, &mut browser).await;
    assert_eq!(first.receipt.status(), Settlement::Enqueued);
    let third = channel.submit(Command::Status, deadline()).unwrap();
    let c = request(&mut browser).await;
    assert_eq!(c["id"], 3);
    assert_eq!(third.receipt.status(), Settlement::Enqueued);
    send(
        &mut browser,
        json!({"type":"success","id":a["id"],"result":{"which":"first"}}),
    )
    .await;
    send(
        &mut browser,
        json!({"type":"success","id":c["id"],"result":{"which":"third"}}),
    )
    .await;
    assert_eq!(first.wait().await.unwrap().value["which"], "first");
    assert_eq!(third.wait().await.unwrap().value["which"], "third");
    assert_eq!(channel.snapshot()["ignored_replies"], 2);
    finish(&channel, events).await;
}

#[tokio::test]
async fn queue_refusal_and_frame_limit_do_not_mark_or_leak_effects() {
    let (mut channel, events, _browser) = fixture().await;
    let _original = channel.outgoing.clone();
    let (queue, _reader) = mpsc::channel(1);
    queue.try_send("occupied".into()).unwrap();
    channel.outgoing = queue;
    let (client, effects) = channel.track_effects();
    assert!(matches!(
        client.submit(Command::CreateTab, deadline()),
        Err(Error::Rejected(Failure {
            code: "browser_busy",
            state: "not_sent"
        }))
    ));
    assert!(matches!(
        client.submit(
            Command::TestPayload {
                effect: Effect::Page,
                text: "x".repeat(MAX_OUTGOING)
            },
            deadline()
        ),
        Err(Error::Rejected(Failure {
            code: "browser_invalid_params",
            state: "not_sent"
        }))
    ));
    assert!(!effects.load(Ordering::Acquire));
    assert_eq!(channel.snapshot()["pending"], 0);
    assert_eq!(channel.snapshot()["free_slots"], SLOTS);
    finish(&channel, events).await;
}

#[tokio::test]
async fn full_slots_keep_work_but_cannot_block_reserved_close() {
    let (channel, events, mut browser) = fixture().await;
    let (client, effects) = channel.track_effects();
    let mut receipts = Vec::new();
    // No await: every command is queued before either transport task can poll.
    for _ in 0..SLOTS {
        let call = client.submit(Command::CreateTab, deadline()).unwrap();
        receipts.push(call.receipt.clone());
        drop(call);
    }
    assert_eq!(channel.snapshot()["pending"], SLOTS);
    assert!(matches!(
        client.submit(Command::CreateTab, deadline()),
        Err(Error::Rejected(Failure {
            code: "browser_busy",
            ..
        }))
    ));
    channel.graceful_stop();
    assert!(!channel.connected());
    assert_eq!(channel.pending_effects(), SLOTS);
    assert_eq!(
        request(&mut browser).await,
        json!({"id":0,"method":"browser.close","params":{}})
    );
    assert!(receipts.iter().all(|r| r.status() == Settlement::Enqueued));
    browser.close(None).await.unwrap();
    assert_eq!(stopped(&channel).await, Fault::Disconnected);
    assert!(
        receipts
            .iter()
            .all(|r| r.status() == Settlement::Unknown(Fault::Disconnected))
    );
    assert!(effects.load(Ordering::Acquire));
    finish(&channel, events).await;
}

#[tokio::test]
async fn event_overflow_fences_before_the_consumer_receives_anything() {
    let (channel, events, mut browser) = fixture().await;
    let call = channel.submit(Command::CreateTab, deadline()).unwrap();
    let receipt = call.receipt.clone();
    request(&mut browser).await;
    for _ in 0..=EVENT_COUNT {
        event(&mut browser, "unconsumed").await;
    }
    assert_eq!(stopped(&channel).await, Fault::EventGap);
    assert_eq!(events.delivered(), 0);
    assert_eq!(events.queue.len(), EVENT_COUNT);
    assert!(matches!(
        channel.submit(Command::CreateTab, deadline()),
        Err(Error::Rejected(Failure {
            code: "browser_lost",
            ..
        }))
    ));
    assert_eq!(receipt.status(), Settlement::Unknown(Fault::EventGap));
    assert!(matches!(
        call.wait().await,
        Err(Error::Transport(Fault::EventGap))
    ));
    finish(&channel, events).await;
}

#[tokio::test]
async fn critical_event_bytes_and_missing_consumer_are_independent_limits() {
    for size in [60 * 1024, MAX_EVENT + 1] {
        let (channel, events, mut browser) = fixture().await;
        let frame = json!({"type":"event","method":CORE_EVENTS[0],"params":{"context":"x","parent":null,"padding":"x".repeat(size)}});
        let total = if size > MAX_EVENT {
            1
        } else {
            EVENT_BYTES / frame.to_string().len() + 1
        };
        for _ in 0..total {
            send(&mut browser, frame.clone()).await;
        }
        assert_eq!(stopped(&channel).await, Fault::EventGap);
        assert!(events.queue.len() < EVENT_COUNT);
        assert!(channel.snapshot()["event_bytes"].as_u64().unwrap() <= EVENT_BYTES as u64);
        finish(&channel, events).await;
    }
    let (channel, events, _browser) = fixture().await;
    drop(events);
    assert!(!channel.connected());
    assert_eq!(stopped(&channel).await, Fault::EventGap);
    channel.wait_stopped(deadline()).await.unwrap();
}

#[tokio::test]
async fn unrelated_event_flood_does_not_occupy_the_critical_queue() {
    let (channel, mut events, mut browser) = fixture().await;
    for _ in 0..1000 {
        send(
            &mut browser,
            json!({"type":"event","method":"log.entryAdded","params":{"text":"synthetic"}}),
        )
        .await;
    }
    processed(&mut events, &mut browser).await;
    assert!(channel.connected());
    assert!(events.queue.is_empty());
    assert_eq!(channel.snapshot()["event_bytes"], 0);
    finish(&channel, events).await;
}

#[tokio::test]
async fn navigation_identity_precedes_reply_delivery_without_an_event_consumer() {
    let (channel, events, mut browser) = fixture().await;
    let context = channel.context(&json!("document-context")).unwrap();
    for (method, params, expected) in [
        (
            "browsingContext.navigationCommitted",
            json!({"context":"document-context","navigation":"nav-1"}),
            Some("nav-1"),
        ),
        (
            "browsingContext.navigationCommitted",
            json!({"context":"document-context","navigation":"nav-2"}),
            Some("nav-2"),
        ),
        (
            "browsingContext.contextDestroyed",
            json!({"context":"document-context"}),
            None,
        ),
    ] {
        let call = channel.submit(Command::Status, deadline()).unwrap();
        let request = request(&mut browser).await;
        send(
            &mut browser,
            json!({"type":"event","method":method,"params":params}),
        )
        .await;
        send(
            &mut browser,
            json!({"type":"success","id":request["id"],"result":{}}),
        )
        .await;
        call.wait().await.unwrap();
        assert_eq!(channel.navigation(&context).unwrap().as_deref(), expected);
    }
    let foreign = ContextId::parse(channel.scope() + 1, &json!("document-context")).unwrap();
    assert!(channel.navigation(&foreign).is_err());
    finish(&channel, events).await;
}

#[tokio::test]
async fn navigation_identity_budget_fences_at_the_producer() {
    let (channel, events, mut browser) = fixture().await;
    for index in 0..=super::super::event::MAX_CONTEXTS {
        send(&mut browser,json!({"type":"event","method":"browsingContext.navigationCommitted","params":{"context":format!("context-{index}"),"navigation":"native-navigation"}})).await;
    }
    assert_eq!(stopped(&channel).await, Fault::EventGap);
    assert!(channel.submit(Command::Status, deadline()).is_err());
    finish(&channel, events).await;
}

#[tokio::test]
async fn malformed_envelopes_and_oversized_messages_fail_without_payload_diagnostics() {
    for payload in [
        "not-json",
        r#"{"id":1,"result":{}}"#,
        r#"{"type":"success","id":1,"result":null}"#,
        r#"{"type":"error","id":1,"error":42,"message":"canary"}"#,
        r#"{"type":"event","method":"browsingContext.contextCreated","params":{}}"#,
    ] {
        let (channel, events, mut browser) = fixture().await;
        browser.send(Message::Text(payload.into())).await.unwrap();
        assert_eq!(stopped(&channel).await, Fault::Malformed);
        finish(&channel, events).await;
    }
    let (channel, events, mut browser) = fixture().await;
    let write = tokio::spawn(async move {
        let _ = browser
            .send(Message::Text("x".repeat(MAX_MESSAGE + 1).into()))
            .await;
    });
    assert_eq!(stopped(&channel).await, Fault::MessageLimit);
    write.abort();
    let _ = write.await;
    finish(&channel, events).await;
}

#[tokio::test]
async fn ids_are_scoped_per_connection_and_exhaustion_never_wraps() {
    let (first, events1, mut browser1) = fixture().await;
    let (second, events2, mut browser2) = fixture().await;
    let a = first.submit(Command::Status, deadline()).unwrap();
    let b = second.submit(Command::Status, deadline()).unwrap();
    assert_eq!(request(&mut browser1).await["id"], 1);
    assert_eq!(request(&mut browser2).await["id"], 1);
    send(
        &mut browser2,
        json!({"type":"success","id":1,"result":{"scope":2}}),
    )
    .await;
    assert_eq!(b.wait().await.unwrap().value["scope"], 2);
    assert_eq!(a.receipt.status(), Settlement::Enqueued);
    send(
        &mut browser1,
        json!({"type":"success","id":1,"result":{"scope":1}}),
    )
    .await;
    assert_eq!(a.wait().await.unwrap().value["scope"], 1);
    first.inner.next.store(MAX_ID + 1, Ordering::Relaxed);
    assert!(matches!(
        first.submit(Command::Status, deadline()),
        Err(Error::Rejected(Failure {
            code: "browser_id_exhausted",
            ..
        }))
    ));
    assert!(browser2.next().now_or_never().is_none());
    finish(&first, events1).await;
    finish(&second, events2).await;
}

#[tokio::test]
async fn startup_subscribes_before_snapshot_and_reconciles_early_destruction() {
    let (channel, mut events, mut browser) = fixture().await;
    let client = channel.clone();
    let boot = tokio::spawn(async move {
        let result = super::super::startup::initialize(&client, &mut events, deadline()).await;
        (result, events)
    });
    let status = request(&mut browser).await;
    assert_eq!(status["method"], "session.status");
    send(
        &mut browser,
        json!({"type":"success","id":status["id"],"result":{"ready":true}}),
    )
    .await;
    let new = request(&mut browser).await;
    assert_eq!(new["method"], "session.new");
    send(&mut browser,json!({"type":"success","id":new["id"],"result":{"sessionId":"fixture","capabilities":{"browserName":"firefox","browserVersion":"160.0"}}})).await;
    let subscribe = request(&mut browser).await;
    assert_eq!(subscribe["method"], "session.subscribe");
    assert!(subscribe["params"].get("contexts").is_none());
    event(&mut browser, "old").await;
    send(
        &mut browser,
        json!({"type":"success","id":subscribe["id"],"result":{"subscription":"fixture"}}),
    )
    .await;
    let tree = request(&mut browser).await;
    assert_eq!(tree["method"], "browsingContext.getTree");
    send(
        &mut browser,
        json!({"type":"event","method":CORE_EVENTS[1],"params":{"context":"old"}}),
    )
    .await;
    event(&mut browser, "new").await;
    send(&mut browser,json!({"type":"success","id":tree["id"],"result":{"contexts":[{"context":"old","parent":null,"children":[]}]}})).await;
    let (boot, events) = boot.await.unwrap();
    let boot = boot.unwrap();
    assert_eq!(
        boot.contexts
            .ids()
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        vec!["new"]
    );
    assert_eq!(
        (boot.product.as_str(), boot.version.as_str()),
        ("firefox", "160.0")
    );
    finish(&channel, events).await;
}
