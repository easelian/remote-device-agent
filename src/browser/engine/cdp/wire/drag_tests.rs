use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

fn fixture() -> (Cdp, BufReader<DuplexStream>) {
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let cdp = Cdp::new(read, write);
    cdp.test_sessions(&["page"]);
    (cdp, BufReader::new(browser))
}
fn deadline() -> Instant {
    (tokio::time::Instant::now() + Duration::from_secs(5)).into_std()
}
async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
    let mut frame = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), browser.read_until(0, &mut frame))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.pop(), Some(0));
    serde_json::from_slice(&frame).unwrap()
}
async fn reply(browser: &mut BufReader<DuplexStream>, id: &Value, value: Value) {
    let mut frame = serde_json::to_vec(&json!({"id":id,"result":value})).unwrap();
    frame.push(0);
    browser.get_mut().write_all(&frame).await.unwrap();
}

#[tokio::test]
async fn takeover_fences_old_epoch_release_after_press() {
    let (cdp, mut browser) = fixture();
    let control = Arc::new(Control::new());
    let old = cdp.authorized(control.clone(), 1);
    let press = tokio::spawn(async move {
        old.call(
            Some("page"),
            Command::InputMousePress,
            json!({"type":"mousePressed","x":1,"y":1,"button":"left","buttons":1,"clickCount":1}),
            deadline(),
        )
        .await
    });
    let sent = request(&mut browser).await;
    assert_eq!(sent["method"], "Input.dispatchMouseEvent");
    reply(&mut browser, &sent["id"], json!({})).await;
    assert!(press.await.unwrap().is_ok());
    control.change(true);
    let old = cdp.authorized(control.clone(), 1);
    let error = old
        .call(
            Some("page"),
            Command::InputDragDrop,
            json!({"type":"mouseReleased","x":1,"y":1,"button":"left","buttons":0,"clickCount":1}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "browser_control_changed");
    assert_eq!(cdp.pending(), 0);
    cdp.stop();
}

#[tokio::test]
async fn cancel_failure_does_not_issue_release() {
    let (cdp, mut browser) = fixture();
    let cancel = tokio::spawn({
        let cdp = cdp.clone();
        async move { cdp.cancel_drag("page").await }
    });
    let sent = request(&mut browser).await;
    assert_eq!(sent["method"], "Input.cancelDragging");
    let mut frame = serde_json::to_vec(
        &json!({"id":sent["id"],"error":{"code":"Input.cancelDragging failed"}}),
    )
    .unwrap();
    frame.push(0);
    browser.get_mut().write_all(&frame).await.unwrap();
    assert_eq!(
        cancel.await.unwrap().unwrap_err().code,
        "browser_cleanup_pending"
    );
    assert!(!cdp.connected());
    let mut leftover = Vec::new();
    if let Ok(Ok(n)) = tokio::time::timeout(
        Duration::from_millis(50),
        browser.read_until(0, &mut leftover),
    )
    .await
    {
        assert_eq!(n, 0, "failed cancellation issued another native command");
    }
    cdp.stop();
}

#[tokio::test]
async fn cancel_success_is_followed_by_release_and_uses_cleanup_capacity() {
    let (cdp, mut browser) = fixture();
    let ordinary = cdp.inner.slots.clone();
    let capacity = ordinary.available_permits() as u32;
    let _occupied = ordinary.try_acquire_many_owned(capacity).unwrap();
    let cancel = tokio::spawn({
        let cdp = cdp.clone();
        async move { cdp.cancel_drag("page").await }
    });
    let first = request(&mut browser).await;
    assert_eq!(first["method"], "Input.cancelDragging");
    reply(&mut browser, &first["id"], json!({})).await;
    let second = request(&mut browser).await;
    assert_eq!(second["method"], "Input.dispatchMouseEvent");
    assert_eq!(second["params"]["type"], "mouseReleased");
    assert_eq!(second["params"]["clickCount"], 0);
    assert_eq!(second["params"]["x"], -1);
    reply(&mut browser, &second["id"], json!({})).await;
    assert!(cancel.await.unwrap().is_ok());
    assert_eq!(cdp.pending(), 0);
    cdp.stop();
}

#[tokio::test]
async fn dialog_blocks_the_drop_even_after_an_admitted_press() {
    let (cdp, mut browser) = fixture();
    let tracked = cdp.track_effects().0;
    let task = tokio::spawn({
        let tracked = tracked.clone();
        async move {
            tracked.call(Some("page"),Command::InputMousePress,json!({"type":"mousePressed","x":1,"y":1,"button":"left","buttons":1,"clickCount":1}),deadline()).await
        }
    });
    let sent = request(&mut browser).await;
    assert_eq!(sent["method"], "Input.dispatchMouseEvent");
    reply(&mut browser, &sent["id"], json!({})).await;
    assert!(task.await.unwrap().is_ok());
    let mut events = cdp.events();
    let mut frame = serde_json::to_vec(
        &json!({"sessionId":"page","method":"Page.javascriptDialogOpening","params":{}}),
    )
    .unwrap();
    frame.push(0);
    browser.get_mut().write_all(&frame).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    let err = tracked
        .call(
            Some("page"),
            Command::InputDragDrop,
            json!({"type":"mouseReleased","x":2,"y":2,"button":"left","buttons":0,"clickCount":1}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "browser_dialog_open");
    cdp.stop();
}

#[tokio::test]
async fn drag_start_settles_the_admitted_position_before_takeover_can_drop() {
    let (wire, mut browser) = fixture();
    let control = Arc::new(Control::new());
    let old = wire.authorized(control.clone(), 1);
    let start = tokio::spawn({
        let old = old.clone();
        async move { old.start_drag("page", 18.0, 8.0, deadline()).await }
    });
    let first = request(&mut browser).await;
    assert_eq!(wire.pending(), 2);
    control.change(true);
    reply(&mut browser, &first["id"], json!({})).await;
    let second = request(&mut browser).await;
    assert_ne!(first["id"], second["id"]);
    assert_eq!(first["params"], second["params"]);
    assert_eq!(second["params"]["type"], "mouseMoved");
    reply(&mut browser, &second["id"], json!({})).await;
    assert!(start.await.unwrap().is_ok());
    let drop = old
        .call(
            Some("page"),
            Command::InputDragDrop,
            json!({"type":"mouseReleased","x":80,"y":8,"button":"left","buttons":0}),
            deadline(),
        )
        .await;
    assert_eq!(drop.unwrap_err().code, "browser_control_changed");
    assert_eq!(wire.pending(), 0);
    wire.stop();
}
