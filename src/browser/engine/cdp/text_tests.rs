use super::*;
use crate::browser::engine::testing::Ids;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
    let mut frame = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), browser.read_until(0, &mut frame))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.pop(), Some(0));
    serde_json::from_slice(&frame).unwrap()
}

async fn send(browser: &mut BufReader<DuplexStream>, value: Value) {
    let mut frame = serde_json::to_vec(&value).unwrap();
    frame.push(0);
    browser.get_mut().write_all(&frame).await.unwrap();
}

#[tokio::test]
async fn cancelled_human_insertion_keeps_input_handoff_blocked_until_reply() {
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let cdp = Engine::from_private_channel(read, write);
    let control = Arc::new(Control::new());
    let epoch = control.change(true);
    let (human, effects) = cdp.human(control, epoch).track_effects();
    let ids = Ids::for_engine(&cdp);
    let focus = Focus {
        frame: ids.frame("page", "frame", "document", "https://fixture.invalid/"),
        node: ids.node(&ids.context("page"), 7),
    };
    let task = tokio::spawn(async move {
        focus
            .insert(
                &human,
                &json!("fixture"),
                Instant::now() + Duration::from_secs(30),
            )
            .await
    });
    let mut browser = BufReader::new(browser);
    for (method, result) in [
        (
            "Page.getFrameTree",
            json!({"frameTree":{"frame":{"id":"frame","loaderId":"document"}}}),
        ),
        ("Page.createIsolatedWorld", json!({"executionContextId":1})),
        ("DOM.resolveNode", json!({"object":{"objectId":"node"}})),
    ] {
        let command = request(&mut browser).await;
        assert_eq!(command["method"], method);
        send(&mut browser, json!({"id":command["id"],"result":result})).await;
    }
    let insert = request(&mut browser).await;
    assert_eq!(insert["method"], "Runtime.callFunctionOn");
    assert_eq!(
        insert["params"]["functionDeclaration"],
        Engine::focus_program()
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // Human release and AI resume both rely on this barrier. Cancelling
    // the caller cannot prove the browser stopped editing the field.
    assert_eq!(cdp.pending(), 1);
    assert!(effects.load(Ordering::Acquire));
    let barriers = cdp.test_metrics()["barriers"].as_u64().unwrap();
    send(
        &mut browser,
        json!({"id":insert["id"],"result":{"result":{"value":"inserted"}}}),
    )
    .await;
    send(&mut browser, json!({"id":0,"result":{}})).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while cdp.test_metrics()["barriers"].as_u64().unwrap() == barriers {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(cdp.pending(), 0);
    assert!(effects.load(Ordering::Acquire));
    cdp.stop();
}
