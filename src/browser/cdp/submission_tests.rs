//! Deterministic queue/fence tests: the endpoint controls responses and Tokio
//! controls deadlines. No sleep is used to guess whether a command was sent.
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

fn fixture() -> (Cdp, BufReader<DuplexStream>) {
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    (Cdp::new(read, write), BufReader::new(browser))
}

fn deadline() -> Instant {
    (tokio::time::Instant::now() + Duration::from_secs(30)).into_std()
}

async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
    let mut frame = Vec::new();
    browser.read_until(0, &mut frame).await.unwrap();
    assert_eq!(frame.pop(), Some(0));
    serde_json::from_slice(&frame).unwrap()
}

async fn send(browser: &mut BufReader<DuplexStream>, value: Value) {
    let mut frame = serde_json::to_vec(&value).unwrap();
    frame.push(0);
    browser.get_mut().write_all(&frame).await.unwrap();
}

// Observing this event proves the reader processed every preceding reply.
async fn processed(cdp: &Cdp, browser: &mut BufReader<DuplexStream>) {
    let mut events = cdp.events();
    send(
        browser,
        json!({"method":"Page.lifecycleEvent","params":{"name":"test_barrier"}}),
    )
    .await;
    loop {
        if events.recv().await.unwrap()["params"]["name"] == "test_barrier" {
            break;
        }
    }
}

fn call(cdp: Cdp, command: Command, params: Value) -> tokio::task::JoinHandle<Result<Value>> {
    tokio::spawn(async move { cdp.call(Some("page"), command, params, deadline()).await })
}

#[tokio::test(start_paused = true)]
async fn enqueued_effect_survives_fence_timeout_and_late_success() {
    let (cdp, mut browser) = fixture();
    let control = Arc::new(Control::new());
    let (authorized, effects) = cdp.authorized(control.clone(), 1).track_effects();
    let task = call(
        authorized.clone(),
        Command::InputInsertText,
        json!({"text":"fixture"}),
    );
    let sent = request(&mut browser).await;
    assert_eq!(sent["method"], "Input.insertText");
    assert_eq!(sent["params"], json!({"text":"fixture"}));
    assert_eq!(sent["sessionId"], "page");
    assert_eq!(cdp.pending(), 1);
    assert!(effects.load(Ordering::Acquire));
    control.change(true);
    let refused = authorized
        .call(Some("page"), Command::DomFocus, json!({}), deadline())
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code, refused.state),
        ("browser_control_changed", "not_sent")
    );
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_timeout_unknown", "unknown")
    );
    assert_eq!(cdp.pending(), 1);
    assert_eq!(cdp.inner.slots.available_permits(), 31);
    send(&mut browser, json!({"id":sent["id"],"result":{}})).await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.pending(), 0);
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    assert!(effects.load(Ordering::Acquire));
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn cancelled_reads_keep_transport_ownership_without_an_input_barrier() {
    let (cdp, mut browser) = fixture();
    let (tracked, effects) = cdp.track_effects();
    let task = call(tracked, Command::RuntimeEvaluate(Effect::None), json!({}));
    let sent = request(&mut browser).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(cdp.pending(), 0);
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 1);
    assert_eq!(cdp.inner.slots.available_permits(), 31);
    assert!(!effects.load(Ordering::Acquire));
    send(
        &mut browser,
        json!({"id":sent["id"],"error":{"code":-32000}}),
    )
    .await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 0);
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn script_effects_and_late_error_ids_do_not_leak_between_operations() {
    let (cdp, mut browser) = fixture();
    let (first, effects) = cdp.track_effects();
    let task = call(first, Command::RuntimeCallFunction(Effect::Page), json!({}));
    let old = request(&mut browser).await;
    assert_eq!(old["method"], "Runtime.callFunctionOn");
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(task.await.unwrap().unwrap_err().state, "unknown");
    assert_eq!(cdp.pending(), 1);
    let (second, new_effects) = cdp.track_effects();
    let next_task = call(
        second,
        Command::RuntimeCallFunction(Effect::None),
        json!({}),
    );
    let next = request(&mut browser).await;
    assert_ne!(old["id"], next["id"]);
    send(
        &mut browser,
        json!({"id":old["id"],"error":{"code":-32000}}),
    )
    .await;
    // A duplicate old reply must not consume or answer the new request.
    send(
        &mut browser,
        json!({"id":old["id"],"result":{"wrong":true}}),
    )
    .await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.pending(), 0);
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 1);
    assert!(!next_task.is_finished());
    assert!(effects.load(Ordering::Acquire));
    assert!(!new_effects.load(Ordering::Acquire));
    send(
        &mut browser,
        json!({"id":next["id"],"result":{"fresh":true}}),
    )
    .await;
    assert_eq!(next_task.await.unwrap().unwrap(), json!({"fresh":true}));
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn saturated_slots_refuse_new_effects_but_preserve_cancelled_work() {
    let (cdp, mut browser) = fixture();
    let mut sent = Vec::new();
    for _ in 0..32 {
        let task = call(cdp.clone(), Command::PageGetFrameTree, json!({}));
        sent.push(request(&mut browser).await);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(cdp.inner.slots.available_permits(), 0);
    assert_eq!(cdp.pending(), 0);
    let (tracked, effects) = cdp.track_effects();
    let error = tracked
        .call(
            Some("page"),
            Command::InputInsertText,
            json!({"text":"fixture"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    assert!(!effects.load(Ordering::Acquire));
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 32);
    for command in sent {
        send(&mut browser, json!({"id":command["id"],"result":{}})).await;
    }
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn failed_enqueue_rolls_back_pending_and_does_not_mark_an_effect() {
    let (mut cdp, _browser) = fixture();
    let _keep_writer_alive = cdp.outgoing.clone();
    let (queue, mut requests) = mpsc::channel(1);
    queue.try_send(vec![7]).unwrap();
    cdp.outgoing = queue;
    let (tracked, effects) = cdp.track_effects();
    let error = tracked
        .call(
            Some("page"),
            Command::PageNavigate,
            json!({"url":"about:blank"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    assert_eq!(cdp.pending(), 0);
    assert!(cdp.inner.pending.lock().unwrap().commands.is_empty());
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    assert!(!effects.load(Ordering::Acquire));
    assert_eq!(requests.try_recv().unwrap(), vec![7]);
    assert!(requests.try_recv().is_err());
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn dialog_blocks_new_input_but_keeps_release_ownership_until_reply() {
    let (cdp, mut browser) = fixture();
    send(
        &mut browser,
        json!({"sessionId":"page","method":"Page.javascriptDialogOpening","params":{}}),
    )
    .await;
    processed(&cdp, &mut browser).await;
    let (tracked, effects) = cdp.track_effects();
    let error = tracked
        .call(
            Some("page"),
            Command::InputKeyPress,
            json!({"type":"keyDown","key":"Enter"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_dialog_open", "not_sent")
    );
    assert_eq!(cdp.pending(), 0);
    assert!(!effects.load(Ordering::Acquire));
    let error = tracked
        .cleanup()
        .call(
            Some("page"),
            Command::InputKeyRelease,
            json!({"type":"keyUp","key":"Enter"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_dialog_open", "unknown")
    );
    let release = request(&mut browser).await;
    assert_eq!(release["method"], "Input.dispatchKeyEvent");
    assert_eq!(release["params"]["type"], "keyUp");
    assert_eq!(cdp.pending(), 1);
    assert!(effects.load(Ordering::Acquire));
    send(&mut browser, json!({"id":release["id"],"result":{}})).await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.pending(), 0);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn paired_input_releases_its_press_after_control_changes() {
    let (cdp, mut browser) = fixture();
    let control = Arc::new(Control::new());
    let (tracked, effects) = cdp.authorized(control.clone(), 1).track_effects();
    let task = tokio::spawn(async move {
        tracked
            .input_pair(
                Some("page"),
                InputPair::Key,
                json!({"type":"keyDown","key":"Enter"}),
                json!({"type":"keyUp","key":"Enter"}),
                deadline(),
            )
            .await
    });
    let press = request(&mut browser).await;
    assert_eq!(press["params"]["type"], "keyDown");
    control.change(true);
    send(&mut browser, json!({"id":press["id"],"result":{}})).await;
    let release = request(&mut browser).await;
    assert_eq!(release["params"]["type"], "keyUp");
    assert_eq!(release["method"], press["method"]);
    assert_ne!(release["id"], press["id"]);
    assert_eq!(cdp.pending(), 1);
    send(&mut browser, json!({"id":release["id"],"result":{}})).await;
    task.await.unwrap().unwrap();
    assert_eq!(cdp.pending(), 0);
    assert!(effects.load(Ordering::Acquire));
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn disconnect_does_not_erase_committed_operation_progress() {
    let (cdp, mut browser) = fixture();
    let (tracked, effects) = cdp.track_effects();
    let task = call(tracked, Command::PageNavigate, json!({"url":"about:blank"}));
    request(&mut browser).await;
    drop(browser);
    let error = task.await.unwrap().unwrap_err();
    assert_eq!((error.code, error.state), ("browser_lost", "unknown"));
    assert!(!cdp.connected());
    assert!(effects.load(Ordering::Acquire));
    assert_eq!(cdp.inner.slots.available_permits(), 32);
}
