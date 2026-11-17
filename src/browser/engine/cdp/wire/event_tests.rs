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
        let wire = Cdp::new(read, write);
        wire.test_sessions(&["page"]);
        Self {
            wire,
            browser: BufReader::new(browser),
        }
    }
    async fn send(&mut self, mut frame: Vec<u8>) {
        frame.push(0);
        self.browser.get_mut().write_all(&frame).await.unwrap();
    }
    async fn json(&mut self, value: Value) {
        self.send(serde_json::to_vec(&value).unwrap()).await;
    }
    async fn barrier(&mut self) {
        let previous = self.wire.inner.barriers.load(Ordering::Acquire);
        self.json(json!({"id":0,"result":{}})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.wire.inner.barriers.load(Ordering::Acquire) == previous {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    async fn request(&mut self) -> Value {
        let mut bytes = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            self.browser.read_until(0, &mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bytes.pop(), Some(0));
        serde_json::from_slice(&bytes).unwrap()
    }
    async fn refused_input(&mut self) {
        let (wire, effect) = self.wire.track_effects();
        let error = wire
            .call(
                Some("page"),
                Command::InputInsertText,
                json!({"text":"never sent"}),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "browser_lost");
        assert!(!effect.load(Ordering::Acquire));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), self.browser.read_u8())
                .await
                .is_err()
        );
    }
    fn usage(&self) -> (usize, usize) {
        (
            events::MAX_EVENTS - self.wire.inner.event_slots.available_permits(),
            events::MAX_EVENT_BYTES - self.wire.inner.event_bytes.available_permits(),
        )
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.wire.stop();
    }
}

fn changed() -> Value {
    json!({"method":"Target.targetInfoChanged","params":{"targetInfo":{"targetId":"tab","type":"page","title":"fixture","url":"about:blank"}}})
}
fn sized_event(bytes: usize) -> Vec<u8> {
    let mut value = json!({"method":"Fetch.requestPaused","sessionId":"page","params":{
        "requestId":"request","frameId":"tab","request":{"url":"https://fixture.invalid/file"},
        "responseStatusCode":200,"responseHeaders":[{"name":"x-fixture","value":""}]}});
    let overhead = serde_json::to_vec(&value).unwrap().len();
    value["params"]["responseHeaders"][0]["value"] = "h".repeat(bytes - overhead).into();
    let frame = serde_json::to_vec(&value).unwrap();
    assert_eq!(frame.len(), bytes);
    frame
}

#[tokio::test]
async fn unread_consumer_is_fenced_at_the_first_event_past_256() {
    let mut peer = Peer::new();
    let mut consumer = peer.wire.events();
    let bytes = serde_json::to_vec(&changed()).unwrap().len();
    for _ in 0..256 {
        peer.json(changed()).await;
    }
    peer.barrier().await;
    assert!(peer.wire.connected());
    assert_eq!(peer.usage(), (256, 256 * bytes));
    peer.json(changed()).await;
    peer.barrier().await;
    assert!(!peer.wire.connected());
    assert_eq!(peer.usage(), (256, 256 * bytes));
    peer.refused_input().await;
    assert_eq!(consumer.recv().await.err().unwrap().code, "browser_lost");
    drop(consumer);
    assert_eq!(peer.usage(), (0, 0));
}

#[tokio::test]
async fn aggregate_encoded_bytes_accept_exact_limit_and_refuse_one_byte_excess() {
    for extra in [0, 1] {
        let mut peer = Peer::new();
        let consumer = peer.wire.events();
        let last = serde_json::to_vec(&changed()).unwrap();
        for _ in 0..3 {
            peer.send(sized_event(MAX_MESSAGE)).await;
        }
        peer.send(sized_event(MAX_MESSAGE - last.len() + extra))
            .await;
        peer.barrier().await;
        assert_eq!(
            peer.usage(),
            (4, events::MAX_EVENT_BYTES - last.len() + extra)
        );
        peer.send(last.clone()).await;
        peer.barrier().await;
        if extra == 0 {
            assert!(peer.wire.connected());
            assert_eq!(peer.usage(), (5, events::MAX_EVENT_BYTES));
        } else {
            assert!(!peer.wire.connected());
            assert_eq!(peer.usage(), (4, events::MAX_EVENT_BYTES - last.len() + 1));
            peer.refused_input().await;
        }
        drop(consumer);
        assert_eq!(peer.usage(), (0, 0));
    }
}

#[tokio::test]
async fn consumers_share_payload_and_budget_until_the_last_held_copy_is_dropped() {
    let mut peer = Peer::new();
    let mut first = peer.wire.events();
    let mut second = peer.wire.events();
    // Whitespace and UTF-8 charge the received encoding, not a reserialized value.
    let frame =
        b" {\"method\":\"Target.targetInfoChanged\",\"params\":{\"fixture\":\"\xe4\xb8\xad\"}} "
            .to_vec();
    let bytes = frame.len();
    peer.send(frame).await;
    let a = first.recv().await.unwrap();
    let b = second.recv().await.unwrap();
    assert!(std::ptr::eq(&*a, &*b));
    assert_eq!(peer.usage(), (1, bytes));
    let held = b.clone();
    drop(a);
    drop(b);
    assert_eq!(peer.usage(), (1, bytes));
    let (ready, started) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _held = held;
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(peer.usage(), (0, 0));
    assert!(peer.wire.connected());
    peer.wire.stop();
}

#[tokio::test]
async fn fully_consumed_but_held_events_still_exhaust_the_count_budget() {
    let mut peer = Peer::new();
    let mut consumer = peer.wire.events();
    let mut held = Vec::new();
    for _ in 0..256 {
        peer.json(changed()).await;
        held.push(consumer.recv().await.unwrap());
    }
    assert_eq!(peer.usage().0, 256);
    peer.json(changed()).await;
    peer.barrier().await;
    peer.refused_input().await;
    drop(held);
    assert_eq!(peer.usage(), (0, 0));
}

#[tokio::test]
async fn missing_or_exited_essential_consumer_fences_without_waiting_for_another_event() {
    let mut absent = Peer::new();
    absent.json(changed()).await;
    absent.barrier().await;
    absent.refused_input().await;
    assert_eq!(absent.usage(), (0, 0));
    let mut exited = Peer::new();
    let first = exited.wire.events();
    let _second = exited.wire.events();
    drop(first);
    assert!(!exited.wire.connected());
    exited.refused_input().await;
    let mut excessive = Peer::new();
    let _a = excessive.wire.events();
    let _b = excessive.wire.events();
    let _c = excessive.wire.events();
    assert!(!excessive.wire.connected());
    excessive.refused_input().await;
}

#[tokio::test]
async fn loss_notifies_unknown_but_keeps_late_object_reply_and_cleanup_ownership() {
    let mut peer = Peer::new();
    let consumer = peer.wire.events();
    let wire = peer.wire.clone();
    let caller = tokio::spawn(async move {
        wire.call(
            Some("page"),
            Command::RuntimeEvaluate(Effect::Page),
            json!({"objectGroup":"event-test"}),
            Instant::now() + Duration::from_secs(10),
        )
        .await
    });
    let command = peer.request().await;
    drop(consumer);
    let error = caller.await.unwrap().unwrap_err();
    assert_eq!((error.code, error.state), ("browser_lost", "unknown"));
    assert_eq!(peer.wire.pending(), 1);
    assert_eq!(peer.wire.test_metrics()["leases"]["reserved_objects"], 2);
    peer.refused_input().await;
    peer.json(json!({"id":command["id"],"result":{"result":{"objectId":"late"}}}))
        .await;
    let cleanup = peer.request().await;
    assert_eq!(cleanup["method"], "Runtime.releaseObject");
    assert_eq!(cleanup["params"]["objectId"], "late");
    assert_eq!(peer.wire.pending(), 0);
    assert_eq!(peer.wire.test_metrics()["leases"]["objects"], 1);
    peer.json(json!({"id":cleanup["id"],"result":{}})).await;
    peer.barrier().await;
    assert_eq!(peer.wire.test_metrics()["pending"], 0);
    assert_eq!(peer.wire.test_metrics()["leases"]["objects"], 0);
    assert_eq!(peer.wire.test_metrics()["leases"]["reserved_objects"], 0);
    assert_eq!(peer.wire.inner.slots.available_permits(), 32);
}

#[tokio::test(start_paused = true)]
async fn missing_late_reply_retains_ownership_until_bounded_transport_shutdown() {
    let mut peer = Peer::new();
    let consumer = peer.wire.events();
    let wire = peer.wire.clone();
    let caller = tokio::spawn(async move {
        wire.call(
            Some("page"),
            Command::InputInsertText,
            json!({}),
            (tokio::time::Instant::now() + Duration::from_secs(10)).into_std(),
        )
        .await
    });
    peer.request().await;
    drop(consumer);
    assert_eq!(caller.await.unwrap().unwrap_err().state, "unknown");
    assert_eq!(peer.wire.pending(), 1);
    tokio::time::advance(Duration::from_secs(4)).await;
    tokio::task::yield_now().await;
    assert!(peer.wire.inner.closed.load(Ordering::Acquire));
    assert_eq!(peer.wire.pending(), 0);
}

#[tokio::test]
async fn discarded_events_cannot_hide_unexpected_native_objects() {
    for event in [
        json!({"method":"Page.lifecycleEvent","params":{"name":"unneeded"}}),
        json!({"method":"Runtime.consoleAPICalled","params":{"args":[{"objectId":"unexpected"}]}}),
    ] {
        let mut peer = Peer::new();
        let object = event["method"] == "Runtime.consoleAPICalled";
        peer.json(event).await;
        peer.barrier().await;
        assert_eq!(peer.wire.connected(), !object);
        assert_eq!(peer.usage(), (0, 0));
        if object {
            peer.refused_input().await;
        }
    }
}
