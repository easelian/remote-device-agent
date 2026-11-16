use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn outbound_frame_budget_checks_the_complete_envelope_before_submission() {
    for extra in [0, 1] {
        let (client, browser) = tokio::io::duplex(512 * 1024);
        let (read, write) = tokio::io::split(client);
        let cdp = Cdp::new(read, write);
        let (worker, effect) = cdp.track_effects();
        let overhead =
            serde_json::to_vec(&json!({"id":1,"method":"Input.insertText","params":{"text":""}}))
                .unwrap()
                .len();
        let task = tokio::spawn(async move {
            worker
                .call(
                    None,
                    Command::InputInsertText,
                    json!({"text":"x".repeat(256 * 1024 - overhead + extra)}),
                    Instant::now() + Duration::from_secs(2),
                )
                .await
        });
        if extra == 0 {
            let mut browser = BufReader::new(browser);
            let mut frame = Vec::new();
            browser.read_until(0, &mut frame).await.unwrap();
            assert_eq!(frame.len(), 256 * 1024 + 1);
            assert!(effect.load(Ordering::Acquire));
            browser
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{}}\0")
                .await
                .unwrap();
            task.await.unwrap().unwrap();
        } else {
            let error = task.await.unwrap().unwrap_err();
            assert_eq!(
                (error.code, error.state),
                ("browser_invalid_params", "not_sent")
            );
            assert!(!effect.load(Ordering::Acquire));
        }
        assert_eq!(cdp.test_metrics()["pending"], 0);
        cdp.stop();
    }
}

#[tokio::test]
async fn malformed_and_oversized_replies_close_and_settle_pending_work() {
    for size in [0, MAX_MESSAGE, MAX_MESSAGE + 1] {
        let (client, browser) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(client);
        let cdp = Cdp::new(read, write);
        let worker = cdp.clone();
        let task = tokio::spawn(async move {
            worker
                .call(
                    None,
                    Command::RuntimeEvaluate(Effect::None),
                    json!({}),
                    Instant::now() + Duration::from_secs(5),
                )
                .await
        });
        let mut browser = BufReader::new(browser);
        let mut command = Vec::new();
        browser.read_until(0, &mut command).await.unwrap();
        let prefix = b"{\"id\":1,\"result\":{\"fixture\":\"";
        let suffix = b"\"}}";
        let mut reply = if size == 0 {
            b"{broken".to_vec()
        } else {
            let mut v = prefix.to_vec();
            v.resize(size - suffix.len(), b'x');
            v.extend_from_slice(suffix);
            v
        };
        reply.push(0);
        let _ = browser.get_mut().write_all(&reply).await;
        let result = task.await.unwrap();
        if size == MAX_MESSAGE {
            assert!(result.is_ok());
            assert!(cdp.connected());
        } else {
            assert_eq!(result.unwrap_err().code, "browser_lost");
            assert!(!cdp.connected());
        }
        assert_eq!(cdp.test_metrics()["pending"], 0);
        assert_eq!(cdp.inner.slots.available_permits(), 32);
        cdp.stop();
    }
}
