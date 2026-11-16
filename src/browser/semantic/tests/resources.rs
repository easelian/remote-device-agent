use super::{
    observation::{self, Scene},
    *,
};

async fn stats<P: Port>(port: &P) -> RegistryStats {
    let mut total = RegistryStats {
        nodes: 0,
        scans: 0,
        pending: 0,
    };
    for frame in port.frames(deadline()).await.unwrap() {
        let Output::Stats(value) = port
            .program(&frame, Program::Stats, deadline())
            .await
            .unwrap()
        else {
            panic!("registry count decoder")
        };
        value.validate().unwrap();
        total.nodes += value.nodes;
        total.scans += value.scans;
        total.pending += value.pending;
    }
    total
}
async fn replacements<P: observation::Harness>(
    port: P,
    iterations: usize,
    mut previous: Option<Reference>,
) -> (Reference, Value) {
    port.scene(Scene::Records).await.unwrap();
    let page = Page::new(port.clone());
    let mut maximum = 0;
    for iteration in 0..iterations {
        if iteration > 0 && iteration % 10 == 0 {
            port.scene(Scene::Records).await.unwrap();
        }
        let read = observation::read(&page).await;
        assert!(read.complete);
        let current = observation::reference(&read, "a");
        if let Some(previous) = previous.take() {
            let error = page
                .act(previous, Action::Click, deadline())
                .await
                .unwrap_err();
            assert_eq!(
                (error.code, error.state),
                ("browser_stale_reference", "not_sent")
            );
        }
        assert!(page.resources().0 <= read.rows.len());
        assert_eq!((page.resources().1, page.resources().2), (0, 0));
        if iteration % 10 == 0 || iteration + 1 == iterations {
            let native = stats(&port).await;
            assert!(native.nodes <= read.rows.len());
            assert_eq!((native.scans, native.pending), (0, 0));
            maximum = maximum.max(native.nodes);
        }
        previous = Some(current);
    }
    let last = previous.unwrap();
    page.act(last.clone(), Action::Click, deadline())
        .await
        .unwrap();
    page.settled(deadline()).await.unwrap();
    page.release(deadline()).await.unwrap();
    let released = stats(&port).await;
    assert_eq!(
        (released.nodes, released.scans, released.pending),
        (0, 0, 0)
    );
    assert_eq!(
        (page.resources().0, page.resources().1, page.resources().2),
        (0, 0, 0)
    );
    let epoch = page.control().snapshot().0;
    page.resume(epoch, true, deadline()).await.unwrap();
    let first = page
        .observe(
            Query {
                filter: Filter::Interactive,
                ..Default::default()
            },
            1,
            64000,
            None,
            deadline(),
        )
        .await
        .unwrap();
    assert!(first.cursor.is_some());
    let stale = first.rows[0].reference.clone().unwrap();
    page.expire_for_test();
    let error = page
        .act(stale, Action::Click, deadline())
        .await
        .unwrap_err();
    assert_eq!(error.code, "browser_stale_reference");
    assert!(
        page.observe(
            Query {
                filter: Filter::Interactive,
                ..Default::default()
            },
            1,
            64000,
            first.cursor,
            deadline()
        )
        .await
        .is_err()
    );
    let expired = stats(&port).await;
    assert_eq!((expired.nodes, expired.scans, expired.pending), (0, 0, 0));
    (
        last,
        json!({"observations":iterations,"max_retained_nodes":maximum,"released":released,"expired":expired,"input_tasks":page.resources().1,"pending_effects":page.resources().2}),
    )
}
#[tokio::test]
async fn fake_ten_thousand_replacements_bound_refs_and_release_expired_scans() {
    replacements(Fake::new(), 10000, None).await;
}

fn cycle_record(label: &str, value: &Value) {
    use std::io::Write;
    let root = PathBuf::from(std::env::var_os("RDH_D3_EVIDENCE").unwrap());
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(format!("{label}-cycles.jsonl")))
        .unwrap();
    writeln!(file, "{value}").unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "100 native Chrome sessions and 10000 observations; explicit evidence directory required"]
async fn cdp_native_resource_cycles() {
    let (url, server) = server().await;
    let root = root("cdp-cycles");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut old = None;
    for iteration in 0..100 {
        let started = Instant::now();
        let mut session = engine::semantic_cdp::Session::launch(
            &browser,
            &root,
            policy::Policy::new(Default::default()),
            deadline(),
        )
        .await
        .unwrap();
        session.navigate(&url, deadline()).await.unwrap();
        let (reference, result) = replacements(session.port.clone(), 100, old.take()).await;
        old = Some(reference);
        let live = session.port.test_metrics();
        assert_eq!(live["pending"], 0);
        assert_eq!(live["native_objects"], 0);
        assert_eq!(live["streams"], 0);
        session.close().await.unwrap();
        let closed = session.port.test_metrics();
        assert_eq!(closed["pending"], 0);
        assert_eq!(closed["native_objects"], 0);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        cycle_record(
            "cdp",
            &json!({"cycle":iteration,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"observation":result,"live_transport":live,"closed_transport":closed,"directories_remaining":0}),
        );
    }
    server.abort();
    fs::remove_dir(root).unwrap();
    record(
        "cdp-resources.json",
        &json!({"sessions":100,"observations_per_session":100,"total_observations":10000,"directories_remaining":0,"foreign_refs_rejected":99}),
    );
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "100 native Firefox sessions and 10000 observations; explicit evidence directory required"]
async fn bidi_native_resource_cycles() {
    let (url, server) = server().await;
    let root = root("bidi-cycles");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut old = None;
    for iteration in 0..100 {
        let started = Instant::now();
        let mut session = engine::bidi::page::Session::open(
            &helper,
            &browser,
            &root,
            policy::Policy::new(Default::default()),
        )
        .await
        .unwrap();
        session.navigate(&url, deadline()).await.unwrap();
        let (reference, result) = replacements(session.port.clone(), 100, old.take()).await;
        old = Some(reference);
        let live = session.port.test_metrics();
        assert_eq!(live["pending"], 0);
        session.close().await.unwrap();
        let closed = session.port.test_metrics();
        assert_eq!(closed["pending"], 0);
        assert_eq!(closed["event_bytes"], 0);
        assert_eq!(closed["workers"], 0);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        cycle_record(
            "bidi",
            &json!({"cycle":iteration,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"observation":result,"live_transport":live,"closed_transport":closed,"directories_remaining":0}),
        );
    }
    server.abort();
    fs::remove_dir(root).unwrap();
    record(
        "bidi-resources.json",
        &json!({"sessions":100,"observations_per_session":100,"total_observations":10000,"directories_remaining":0,"foreign_refs_rejected":99}),
    );
}
