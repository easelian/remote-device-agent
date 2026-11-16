use super::*;
use crate::browser::engine::{Engine, testing::Ids};
#[tokio::test]
async fn elements_are_described_together_and_each_has_its_own_outcome() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    let (client, browser) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(client);
    let engine = Engine::from_private_channel(read, write);
    let ids = Ids::for_engine(&engine);
    let frame = ids.frame("session", "top", "1", "https://shop.test/orders");
    let reader = ids.scan(&frame.session, "reader");
    let mut browser = BufReader::new(browser);
    async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
        let mut frame = Vec::new();
        browser.read_until(0, &mut frame).await.unwrap();
        serde_json::from_slice(&frame[..frame.len() - 1]).unwrap()
    }
    async fn answer(browser: &mut BufReader<DuplexStream>, request: &Value, result: Value) {
        let mut reply = serde_json::to_vec(&json!({"id":request["id"],"result":result})).unwrap();
        reply.push(0);
        browser.get_mut().write_all(&reply).await.unwrap();
    }
    // The page marks the first and the last of four elements as controls. The
    // pretend browser answers nothing until it has the four requests for the
    // elements and the one for the details of those two. Asked one at a time,
    // it would never get the second.
    let pretend = async {
        let mut asked = Vec::new();
        for _ in 0..5 {
            asked.push(request(&mut browser).await);
        }
        let details = asked.pop().unwrap();
        assert_eq!(details["params"]["arguments"][0]["value"], json!([0, 3]));
        // The last element left the page before its details were read.
        answer(
            &mut browser,
            &details,
            json!({"result":{"value":[{"connected":true,"signature":"s"},null]}}),
        )
        .await;
        for handle in &asked {
            assert_eq!(handle["method"], "Runtime.callFunctionOn", "{handle}");
            let index = handle["params"]["arguments"][0]["value"].as_u64().unwrap();
            answer(
                &mut browser,
                handle,
                json!({"result":{"objectId":format!("node-{index}")}}),
            )
            .await;
        }
        asked.clear();
        for _ in 0..4 {
            asked.push(request(&mut browser).await);
        }
        for describe in &asked {
            assert_eq!(describe["method"], "DOM.describeNode", "{describe}");
            let object = describe["params"]["objectId"].as_str().unwrap();
            let index: i64 = object.trim_start_matches("node-").parse().unwrap();
            answer(
                &mut browser,
                describe,
                json!({"node":{"backendNodeId":10 + index}}),
            )
            .await;
        }
        asked.clear();
        for _ in 0..4 {
            asked.push(request(&mut browser).await);
        }
        for tree in &asked {
            assert_eq!(tree["method"], "Accessibility.getPartialAXTree", "{tree}");
            let backend = tree["params"]["backendNodeId"].as_i64().unwrap();
            let node = match backend {
                10 => {
                    json!({"backendDOMNodeId":10,"role":{"value":"button"},"name":{"value":"Save"}})
                }
                // The page does not mark the second, and the browser calls it a link.
                11 => {
                    json!({"backendDOMNodeId":11,"role":{"value":"link"},"name":{"value":"Open"}})
                }
                12 => json!({"backendDOMNodeId":12,"ignored":true}),
                _ => {
                    json!({"backendDOMNodeId":13,"role":{"value":"textbox"},"name":{"value":"Name"}})
                }
            };
            answer(&mut browser, tree, json!({"nodes":[node]})).await;
        }
        // The page is then asked for the details of what the browser alone calls
        // a control.
        let details = request(&mut browser).await;
        assert_eq!(details["method"], "Runtime.callFunctionOn", "{details}");
        assert_eq!(details["params"]["arguments"][0]["value"], json!([1]));
        answer(
            &mut browser,
            &details,
            json!({"result":{"value":[{"connected":true,"signature":"t"}]}}),
        )
        .await;
    };
    let described = found(
        &engine,
        &frame,
        &reader,
        &[0, 1, 2, 3],
        &[0, 3],
        Instant::now() + Duration::from_secs(2),
    );
    let (mut described, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(described, pretend)
    })
    .await
    .expect("the browser must be asked about all four elements at once");
    engine.stop();

    let button = described.remove(&0).unwrap().unwrap().unwrap();
    assert_eq!(
        (button.node, button.role.as_str(), button.name.as_str()),
        (ids.node(&frame.session, 10), "button", "Save")
    );
    assert_eq!(button.data.unwrap()["signature"], "s");
    let link = described.remove(&1).unwrap().unwrap().unwrap();
    assert_eq!(link.role, "link");
    assert_eq!(link.data.unwrap()["signature"], "t");
    // The accessibility tree ignores the third, which the page does not mark.
    let ignored = described.remove(&2).unwrap().unwrap().unwrap();
    assert!(!ignored.exposed);
    assert!(ignored.data.is_none());
    let gone = described.remove(&3).unwrap().err().unwrap();
    assert_eq!(
        (gone.code, gone.state),
        ("browser_stale_reference", "not_sent")
    );
}
