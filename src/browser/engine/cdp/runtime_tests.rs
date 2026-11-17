use super::*;
use crate::browser::engine::testing::Ids;
#[tokio::test]
async fn a_document_the_site_policy_refuses_is_failed_and_noted_on_its_tab() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let cdp = Engine::from_private_channel(read, write);
    cdp.test_sessions(&["page"]);
    let ids = Ids::for_engine(&cdp);
    let tab = Target {
        id: "tab".into(),
        handle: ids.tab("tab"),
        root: ids.frame_id("tab"),
        session: ids.context("page"),
        kind: ContextKind::Page,
        url: String::new(),
        title: String::new(),
        dialog: None,
        ready: true,
        blocked: None,
    };
    let targets = Arc::new(Mutex::new(HashMap::from([("tab".to_owned(), tab)])));
    let gate = Arc::new(Gate::default());
    // Where the names used below lead. A name without an answer is let through.
    gate.lookup
        .resolve_with(crate::browser::sites::resolver(|host: String| async move {
            match host.as_str() {
                "metadata.example" => Some(vec!["169.254.169.254".parse().unwrap()]),
                "public.example" => Some(vec!["93.184.216.34".parse().unwrap()]),
                _ => None,
            }
        }));
    let task = guard(cdp.clone(), targets.clone(), None, gate);
    let mut browser = BufReader::new(browser);
    // Sends one paused document request and returns what the hook did with it.
    async fn paused(browser: &mut BufReader<DuplexStream>, mut request: Value) -> Value {
        request["requestId"] = "document".into();
        let mut event = serde_json::to_vec(
            &json!({"sessionId":"page","method":"Fetch.requestPaused","params":request}),
        )
        .unwrap();
        event.push(0);
        browser.get_mut().write_all(&event).await.unwrap();
        let mut frame = Vec::new();
        browser.read_until(0, &mut frame).await.unwrap();
        let command: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
        let mut reply = serde_json::to_vec(&json!({"id":command["id"],"result":{}})).unwrap();
        reply.push(0);
        browser.get_mut().write_all(&reply).await.unwrap();
        command
    }
    let blocked = || targets.lock().unwrap()["tab"].blocked;
    let metadata = "http://169.254.169.254/latest/meta-data/";
    let scenario = async {
        // The tab's own document, refused before anything was sent.
        let failed = paused(
            &mut browser,
            json!({"frameId":"tab","request":{"url":metadata}}),
        )
        .await;
        assert_eq!(failed["method"], "Fetch.failRequest");
        assert_eq!(failed["params"]["errorReason"], "BlockedByClient");
        assert_eq!(blocked(), Some(false));
        // A frame inside it is refused too, and does not change what the tab says.
        let failed = paused(
            &mut browser,
            json!({"frameId":"child","request":{"url":metadata}}),
        )
        .await;
        assert_eq!(failed["method"], "Fetch.failRequest");
        assert_eq!(blocked(), Some(false));
        // The tab sets out for a page it may open.
        let continued = paused(
            &mut browser,
            json!({"frameId":"tab","request":{"url":"http://127.0.0.1:8080/"}}),
        )
        .await;
        assert_eq!(continued["method"], "Fetch.continueRequest");
        assert_eq!(blocked(), None);
        // That page redirects to the refused address: a request has left by now.
        let failed = paused(
            &mut browser,
            json!({"frameId":"tab","redirectedRequestId":"document","request":{"url":metadata}}),
        )
        .await;
        assert_eq!(failed["method"], "Fetch.failRequest");
        assert_eq!(blocked(), Some(true));
        // A name is judged by where it leads, both when it has to be asked about
        // and when the answer is remembered.
        for _ in 0..2 {
            let failed = paused(
                &mut browser,
                json!({"frameId":"tab","request":{"url":"http://metadata.example/"}}),
            )
            .await;
            assert_eq!(failed["method"], "Fetch.failRequest");
            assert_eq!(blocked(), Some(false));
            for open in ["http://public.example/", "http://unanswered.example/"] {
                let continued = paused(
                    &mut browser,
                    json!({"frameId":"tab","request":{"url":open}}),
                )
                .await;
                assert_eq!(continued["method"], "Fetch.continueRequest", "{open}");
                assert_eq!(blocked(), None, "{open}");
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(2), scenario).await;
    cdp.stop();
    task.await.unwrap();
    result.expect("the hook must answer every paused request");
}

#[tokio::test]
async fn where_private_networks_are_refused_the_address_reached_decides() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let cdp = Engine::from_private_channel(read, write);
    cdp.test_sessions(&["page"]);
    let ids = Ids::for_engine(&cdp);
    let tab = Target {
        id: "tab".into(),
        handle: ids.tab("tab"),
        root: ids.frame_id("tab"),
        session: ids.context("page"),
        kind: ContextKind::Page,
        url: String::new(),
        title: String::new(),
        dialog: None,
        ready: true,
        blocked: None,
    };
    let targets = Arc::new(Mutex::new(HashMap::from([("tab".to_owned(), tab)])));
    let gate = Arc::new(Gate {
        sites: serde_json::from_value(json!({"private_networks":"deny"})).unwrap(),
        lookup: Lookup::default(),
    });
    // Every name looks public when it is asked about.
    gate.lookup
        .resolve_with(crate::browser::sites::resolver(|_| async {
            Some(vec!["93.184.216.34".parse().unwrap()])
        }));
    let task = guard(cdp.clone(), targets.clone(), None, gate);
    let mut browser = BufReader::new(browser);
    async fn event(browser: &mut BufReader<DuplexStream>, method: &str, params: Value) {
        let mut event =
            serde_json::to_vec(&json!({"sessionId":"page","method":method,"params":params}))
                .unwrap();
        event.push(0);
        browser.get_mut().write_all(&event).await.unwrap();
    }
    // Reads the hook's next command and acknowledges it.
    async fn command(browser: &mut BufReader<DuplexStream>) -> Value {
        let mut frame = Vec::new();
        browser.read_until(0, &mut frame).await.unwrap();
        let command: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
        let mut reply = serde_json::to_vec(&json!({"id":command["id"],"result":{}})).unwrap();
        reply.push(0);
        browser.get_mut().write_all(&reply).await.unwrap();
        command
    }
    let blocked = || targets.lock().unwrap()["tab"].blocked;
    let request = |network: &str| json!({"requestId":network,"networkId":network,"frameId":"tab","request":{"url":"http://named.example/"}});
    let response = |network: &str| {
        let mut paused = request(network);
        paused["responseStatusCode"] = 200.into();
        paused
    };
    let said =
        |network: &str, space: &str| json!({"requestId":network,"resourceIPAddressSpace":space});
    let scenario = async {
        // The name passes its lookup, and the request is sent.
        event(&mut browser, "Fetch.requestPaused", request("a")).await;
        assert_eq!(
            command(&mut browser).await["method"],
            "Fetch.continueRequest"
        );
        // The browser reached a private address: the page does not get the response.
        event(
            &mut browser,
            "Network.responseReceivedExtraInfo",
            said("a", "Local"),
        )
        .await;
        event(&mut browser, "Fetch.requestPaused", response("a")).await;
        let failed = command(&mut browser).await;
        assert_eq!(failed["method"], "Fetch.failRequest");
        assert_eq!(failed["params"]["errorReason"], "BlockedByClient");
        assert_eq!(blocked(), Some(true));
        // The same when the browser's word arrives after the response is paused.
        event(&mut browser, "Fetch.requestPaused", request("b")).await;
        assert_eq!(
            command(&mut browser).await["method"],
            "Fetch.continueRequest"
        );
        assert_eq!(blocked(), None);
        event(&mut browser, "Fetch.requestPaused", response("b")).await;
        event(
            &mut browser,
            "Network.responseReceivedExtraInfo",
            said("b", "Loopback"),
        )
        .await;
        assert_eq!(command(&mut browser).await["method"], "Fetch.failRequest");
        assert_eq!(blocked(), Some(true));
        // A public address is let through.
        event(&mut browser, "Fetch.requestPaused", request("c")).await;
        assert_eq!(
            command(&mut browser).await["method"],
            "Fetch.continueRequest"
        );
        event(
            &mut browser,
            "Network.responseReceivedExtraInfo",
            said("c", "Public"),
        )
        .await;
        event(&mut browser, "Fetch.requestPaused", response("c")).await;
        assert_eq!(
            command(&mut browser).await["method"],
            "Fetch.continueRequest"
        );
        assert_eq!(blocked(), None);
        // What was said about an earlier hop does not count for the next one, and a
        // response the browser says nothing about goes by the name's lookup.
        event(
            &mut browser,
            "Network.responseReceivedExtraInfo",
            said("d", "Local"),
        )
        .await;
        event(&mut browser, "Fetch.requestPaused", request("d")).await;
        assert_eq!(
            command(&mut browser).await["method"],
            "Fetch.continueRequest"
        );
        event(&mut browser, "Fetch.requestPaused", response("d")).await;
        assert_eq!(
            command(&mut browser).await["method"],
            "Fetch.continueRequest"
        );
        assert_eq!(blocked(), None);
    };
    let result = tokio::time::timeout(Duration::from_secs(3), scenario).await;
    cdp.stop();
    task.await.unwrap();
    result.expect("the hook must answer every paused request");
}

#[tokio::test]
async fn suspended_non_page_target_cannot_block_page_request_progress() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let cdp = Engine::from_private_channel(read, write);
    cdp.test_sessions(&["page"]);
    let task = guard(
        cdp.clone(),
        Arc::new(Mutex::new(HashMap::new())),
        None,
        Arc::new(Gate::default()),
    );
    let mut browser = BufReader::new(browser);
    let scenario = async {
        browser.get_mut().write_all(br#"{"method":"Target.attachedToTarget","params":{"sessionId":"worker","targetInfo":{"targetId":"worker-target","type":"service_worker"}}}"#).await.unwrap();
        browser.get_mut().write_all(&[0]).await.unwrap();
        let mut frame = Vec::new();
        browser.read_until(0, &mut frame).await.unwrap();
        let resume: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(resume["method"], "Runtime.runIfWaitingForDebugger");
        // Renderer startup cannot finish until a document request is released.
        // A browser may deliver this dependency before acknowledging resume.
        browser.get_mut().write_all(br#"{"sessionId":"page","method":"Fetch.requestPaused","params":{"requestId":"navigation","request":{"url":"http://127.0.0.1/fixture"}}}"#).await.unwrap();
        browser.get_mut().write_all(&[0]).await.unwrap();
        frame.clear();
        browser.read_until(0, &mut frame).await.unwrap();
        let continued: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(continued["method"], "Fetch.continueRequest");
        assert_eq!(continued["params"]["requestId"], "navigation");
        for request in [continued, resume] {
            let mut reply = serde_json::to_vec(&json!({"id":request["id"],"result":{}})).unwrap();
            reply.push(0);
            browser.get_mut().write_all(&reply).await.unwrap();
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(1), scenario).await;
    cdp.stop();
    task.await.unwrap();
    result.expect("target resume must not block the request that lets startup finish");
}
