//! Exact envelope sizes exercise the production WebSocket reader and admission.
use super::*;

#[tokio::test]
async fn outgoing_json_envelope_accepts_its_last_byte_and_refuses_one_more() {
    for extra in [0, 1] {
        let (channel, events, mut browser) = fixture().await;
        let (client, effects) = channel.track_effects();
        let overhead = json!({"id":1,"method":"test.payload","params":{"text":""}})
            .to_string()
            .len();
        let call = client.submit(
            Command::TestPayload {
                effect: Effect::Page,
                text: "x".repeat(MAX_OUTGOING - overhead + extra),
            },
            deadline(),
        );
        if extra == 0 {
            let call = call.unwrap();
            let sent = request(&mut browser).await;
            assert_eq!(sent.to_string().len(), MAX_OUTGOING);
            assert!(effects.load(Ordering::Acquire));
            send(
                &mut browser,
                json!({"type":"success","id":sent["id"],"result":{}}),
            )
            .await;
            call.wait().await.unwrap();
        } else {
            assert!(matches!(
                call,
                Err(Error::Rejected(Failure {
                    code: "browser_invalid_params",
                    state: "not_sent"
                }))
            ));
            assert!(!effects.load(Ordering::Acquire));
        }
        assert_eq!(channel.snapshot()["pending"], 0);
        assert_eq!(channel.snapshot()["free_slots"], SLOTS);
        finish(&channel, events).await;
    }
}

#[tokio::test]
async fn incoming_json_envelope_accepts_two_mib_and_fences_the_next_byte() {
    for extra in [0, 1] {
        let (channel, events, mut browser) = fixture().await;
        let call = channel.submit(Command::CreateTab, deadline()).unwrap();
        let receipt = call.receipt.clone();
        let sent = request(&mut browser).await;
        let mut response = json!({"type":"success","id":sent["id"],"result":{"padding":""}});
        let overhead = response.to_string().len();
        response["result"]["padding"] = "x".repeat(MAX_MESSAGE - overhead + extra).into();
        let text = response.to_string();
        assert_eq!(text.len(), MAX_MESSAGE + extra);
        let writer = tokio::spawn(async move { browser.send(Message::Text(text.into())).await });
        if extra == 0 {
            let value = call.wait().await.unwrap();
            assert_eq!(
                value.value["padding"].as_str().unwrap().len(),
                MAX_MESSAGE - overhead
            );
            assert_eq!(receipt.status(), Settlement::Success);
            writer.await.unwrap().unwrap();
        } else {
            assert_eq!(stopped(&channel).await, Fault::MessageLimit);
            assert!(matches!(
                call.wait().await,
                Err(Error::Transport(Fault::MessageLimit))
            ));
            assert_eq!(receipt.status(), Settlement::Unknown(Fault::MessageLimit));
            writer.abort();
            let _ = writer.await;
        }
        finish(&channel, events).await;
    }
}

fn sized_event(bytes: usize) -> Value {
    let mut event = json!({"type":"event","method":CORE_EVENTS[0],"params":{"context":"bounded","parent":null,"padding":""}});
    let overhead = event.to_string().len();
    event["params"]["padding"] = "x".repeat(bytes - overhead).into();
    assert_eq!(event.to_string().len(), bytes);
    event
}

#[tokio::test]
async fn critical_event_byte_leases_fill_exactly_reuse_and_fence_without_consuming() {
    let (channel, mut events, mut browser) = fixture().await;
    let frame = sized_event(MAX_EVENT);
    let count = EVENT_BYTES / MAX_EVENT;
    for _ in 0..count {
        send(&mut browser, frame.clone()).await;
    }
    let until = deadline();
    while events.queue.len() != count {
        assert!(Instant::now() < until && channel.connected());
        tokio::task::yield_now().await;
    }
    assert_eq!(channel.snapshot()["event_bytes"], EVENT_BYTES);
    assert_eq!(events.delivered(), 0);
    events.recv().await.unwrap();
    assert_eq!(channel.snapshot()["event_bytes"], EVENT_BYTES - MAX_EVENT);
    send(&mut browser, frame).await;
    while events.queue.len() != count {
        assert!(Instant::now() < until && channel.connected());
        tokio::task::yield_now().await;
    }
    assert_eq!(channel.snapshot()["event_bytes"], EVENT_BYTES);
    event(&mut browser, "next-event").await;
    assert_eq!(stopped(&channel).await, Fault::EventGap);
    assert_eq!(events.delivered(), 1);
    assert_eq!(events.queue.len(), count);
    assert_eq!(channel.snapshot()["event_bytes"], EVENT_BYTES);
    finish(&channel, events).await;

    let (channel, events, mut browser) = fixture().await;
    send(&mut browser, sized_event(MAX_EVENT + 1)).await;
    assert_eq!(stopped(&channel).await, Fault::EventGap);
    assert_eq!(events.queue.len(), 0);
    assert_eq!(channel.snapshot()["event_bytes"], 0);
    finish(&channel, events).await;
}
