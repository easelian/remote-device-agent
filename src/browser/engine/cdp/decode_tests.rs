use super::super::testing::Ids;
use super::*;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[test]
fn script_failures_and_non_finite_geometry_are_content_free_errors() {
    for value in [
        json!({"exceptionDetails":{"text":"SYNTHETIC_SECRET"},"result":{"value":[]}}),
        json!({"result":{"type":"undefined"}}),
        json!({"result":{}}),
        Value::Null,
    ] {
        let error = script_value(value).unwrap_err();
        assert_eq!(
            (error.code, error.state),
            ("browser_stale_reference", "not_sent")
        );
        assert!(!format!("{error:?}").contains("SYNTHETIC_SECRET"));
    }
    assert_eq!(
        script_value(json!({"result":{"value":null}})).unwrap(),
        Value::Null
    );
    for value in [
        json!({"pageX":0,"pageY":0,"clientWidth":"NaN","clientHeight":720}),
        json!({"pageX":0,"pageY":0,"clientWidth":1280}),
        Value::Null,
    ] {
        assert!(page::viewport(&value).is_err());
    }
}

#[tokio::test]
async fn truncated_script_batches_fail_instead_of_becoming_empty_observations() {
    for payload in [
        json!({"exceptionDetails":{},"result":{"value":[]}}),
        json!({"result":{"type":"undefined"}}),
        json!({"result":{"value":null}}),
        json!({"result":{"value":{"nodes":[]}}}),
        json!({"result":{"value":{"nodes":[{}],"scanned":1,"matches":0,"omitted":[],"changed":false}}}),
    ] {
        let (client, browser) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let engine = Engine::from_private_channel(read, write);
        let ids = Ids::for_engine(&engine);
        let scan = ids.scan(&ids.context("page"), "scan");
        let reply = async {
            let mut browser = BufReader::new(browser);
            let mut bytes = Vec::new();
            browser.read_until(0, &mut bytes).await.unwrap();
            bytes.pop();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            let mut bytes =
                serde_json::to_vec(&json!({"id":request["id"],"result":payload})).unwrap();
            bytes.push(0);
            browser.get_mut().write_all(&bytes).await.unwrap();
            browser
        };
        let (result, _browser) = tokio::join!(
            engine.collect(&scan, 200, 5000, Instant::now() + Duration::from_secs(2)),
            reply
        );
        let error = result.err().expect("malformed batch must fail");
        assert_eq!(
            (error.code, error.state),
            ("browser_stale_reference", "not_sent")
        );
        engine.stop();
    }
}

#[tokio::test]
async fn foreign_handles_and_revoked_scans_never_submit_a_command() {
    let (client, _browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let engine = Engine::from_private_channel(read, write);
    let foreign = Ids::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    assert_eq!(
        engine
            .frames(&foreign.context("page"), deadline)
            .await
            .err()
            .unwrap()
            .code,
        "browser_stale_reference"
    );
    assert_eq!(
        engine
            .focus(&foreign.node(&foreign.context("page"), 7), deadline)
            .await
            .unwrap_err()
            .state,
        "not_sent"
    );
    let ids = Ids::for_engine(&engine);
    let scan = ids.scan(&ids.context("page"), "reader");
    engine.close_reader(&scan, Instant::now()).await;
    assert_eq!(
        engine
            .reader_status(&scan, deadline)
            .await
            .err()
            .unwrap()
            .code,
        "browser_stale_reference"
    );
    assert_eq!(engine.backend.wire.command_count(), 0);
    assert_eq!(engine.test_metrics()["pending"], 0);
    engine.stop();
}

#[tokio::test]
async fn critical_event_gap_closes_the_engine_before_more_input_or_disclosure() {
    let (client, mut browser) = tokio::io::duplex(128 * 1024);
    let (read, write) = tokio::io::split(client);
    let engine = Engine::from_private_channel(read, write);
    let mut events = engine.events();
    let mut raw = engine.raw_test_events();
    for _ in 0..300 {
        browser.write_all(b"{\"method\":\"Target.targetInfoChanged\",\"params\":{\"targetInfo\":{\"targetId\":\"tab\"}}}\0").await.unwrap();
    }
    // A marker from the same reader establishes that the ring was overrun.
    browser
        .write_all(b"{\"method\":\"fixture.marker\"}\0")
        .await
        .unwrap();
    loop {
        match raw.recv().await {
            Ok(v) if v["method"] == "fixture.marker" => break,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(events.recv().await.err().unwrap().code, "browser_lost");
    assert!(!engine.connected());
    let ids = Ids::for_engine(&engine);
    assert_eq!(
        engine
            .insert_text(
                &ids.context("page"),
                "fixture",
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .unwrap_err()
            .code,
        "browser_lost"
    );
}

#[tokio::test]
async fn viewport_image_budget_refuses_the_first_excess_encoded_byte() {
    for size in [
        crate::browser::image::MAX_ENCODED,
        crate::browser::image::MAX_ENCODED + 1,
    ] {
        let (client, browser) = tokio::io::duplex(1024 * 1024);
        let (read, write) = tokio::io::split(client);
        let engine = Engine::from_private_channel(read, write);
        let ids = Ids::for_engine(&engine);
        let context = ids.context("page");
        let reply = async {
            let mut browser = BufReader::new(browser);
            for result in [
                json!({"cssVisualViewport":{"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720}}),
                json!({"data":"A".repeat(size)}),
            ] {
                let mut bytes = Vec::new();
                browser.read_until(0, &mut bytes).await.unwrap();
                bytes.pop();
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                let mut bytes =
                    serde_json::to_vec(&json!({"id":request["id"],"result":result})).unwrap();
                bytes.push(0);
                browser.get_mut().write_all(&bytes).await.unwrap();
            }
            browser
        };
        let (result, _browser) = tokio::join!(
            crate::browser::image::capture(
                &engine,
                &context,
                Instant::now() + Duration::from_secs(5)
            ),
            reply
        );
        if size == crate::browser::image::MAX_ENCODED {
            assert_eq!(result.unwrap()["data"].as_str().unwrap().len(), size);
        } else {
            assert_eq!(result.unwrap_err().code, "browser_image_too_large");
        }
        assert_eq!(engine.test_metrics()["pending"], 0);
        engine.stop();
    }
}
