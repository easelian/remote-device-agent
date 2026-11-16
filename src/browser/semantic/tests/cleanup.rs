use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "20 real Chrome keyDown disconnects with owned process cleanup"]
async fn cdp_native_input_disconnect_cleanup() {
    let (url, server) = server().await;
    let root = root("cdp-disconnect");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut results = Vec::new();
    for _ in 0..20 {
        let mut session = engine::semantic_cdp::Session::launch(
            &browser,
            &root,
            policy::Policy::new(Default::default()),
            deadline(),
        )
        .await
        .unwrap();
        session.navigate(&url, deadline()).await.unwrap();
        results.push(faults::assert_connection_lost(session.port.clone()).await);
        session.close().await.unwrap();
        assert_eq!(session.port.test_metrics()["pending"], 0);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }
    record(
        "cdp-disconnect.json",
        &json!({"cases":results,"directories_remaining":0}),
    );
    server.abort();
    fs::remove_dir(root).unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "20 real Firefox keyDown disconnects with owned process cleanup"]
async fn bidi_native_input_disconnect_cleanup() {
    let (url, server) = server().await;
    let root = root("bidi-disconnect");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut results = Vec::new();
    for _ in 0..20 {
        let mut session = engine::bidi::page::Session::open(
            &helper,
            &browser,
            &root,
            policy::Policy::new(Default::default()),
        )
        .await
        .unwrap();
        session.navigate(&url, deadline()).await.unwrap();
        results.push(faults::assert_connection_lost(session.port.clone()).await);
        session.close().await.unwrap();
        let snapshot = session.port.test_metrics();
        for key in ["pending", "workers", "event_bytes"] {
            assert_eq!(snapshot[key], 0);
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }
    record(
        "bidi-disconnect.json",
        &json!({"cases":results,"directories_remaining":0}),
    );
    server.abort();
    fs::remove_dir(root).unwrap();
}
