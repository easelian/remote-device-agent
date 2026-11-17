//! Same-read oracles from real protocol adapters. These explicit fixtures call
//! the production v2 observation and cache code; public MCP acceptance covers
//! the authenticated dispatch and final runtime control fence separately.
use super::*;
use crate::browser::{
    engine,
    runtime::v2::{Session, native as port, observe},
};
use futures_util::FutureExt;
use std::{fs, io::Write, path::PathBuf};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(15)
}

struct Fixture {
    session: Session,
    port: port::Port,
    page: sem::Page<port::Port>,
    directory: PathBuf,
    backend: &'static str,
    id: String,
}
impl Fixture {
    async fn new(backend: &'static str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("rdh-delta-{backend}-{}", token().unwrap()));
        crate::storage::create_private_dir(&directory).unwrap();
        let policy = sem::policy::Policy::new(Default::default());
        let native = if backend == "cdp" {
            port::Session::Cdp(
                engine::semantic_cdp::Session::launch(
                    &PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap()),
                    &directory,
                    policy,
                    deadline(),
                )
                .await
                .unwrap(),
            )
        } else {
            port::Session::Bidi(
                engine::bidi::page::Session::open(
                    &PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap()),
                    &PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap()),
                    &directory,
                    policy,
                )
                .await
                .unwrap(),
            )
        };
        let port = native.port();
        let session = Session {
            native: Some(native),
            active: port.tab_id().unwrap(),
            references: HashMap::new(),
            reference_tab: None,
            frames: HashMap::new(),
            cursor: None,
            presentation: Cache::default(),
            human: None,
            touched: Instant::now(),
        };
        Self {
            page: sem::Page::new(port.clone()),
            port,
            session,
            directory,
            backend,
            id: token().unwrap(),
        }
    }
    async fn fixture(&self, script: &str) -> Value {
        match &self.port {
            port::Port::Cdp(p) => p.fixture(script, deadline()).await.unwrap(),
            port::Port::Bidi(p) => p.fixture(script, deadline()).await.unwrap(),
        }
    }
    fn metrics(&self) -> Value {
        match &self.port {
            port::Port::Cdp(p) => p.test_metrics(),
            port::Port::Bidi(p) => p.test_metrics(),
        }
    }
    async fn navigate(&self, url: &str) {
        self.port.navigate(url, deadline()).await.unwrap();
        for _ in 0..100 {
            if self.fixture("window.deltaReady===true && Array.from(document.querySelectorAll('iframe')).every(f=>f.contentWindow.deltaReady===true)").await == true { return; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("native corpus did not become ready");
    }
    async fn read(&mut self, base: Option<&str>, capture: bool, presentation: bool) -> Value {
        let epoch = self.port.coordination().control().snapshot().0;
        let mut args = json!({"session_id":self.id,"tab_id":self.port.tab_id().unwrap(),"control_epoch":epoch,"max_nodes":1000,"max_chars":64000});
        if presentation {
            args["presentation"] = json!({"version":1,"mode":"delta"});
            if let Some(base) = base {
                args["presentation"]["base_revision"] = base.into();
            }
        }
        self.session.presentation.capture_oracle = capture;
        let before = self.metrics()["commands"].as_u64().unwrap();
        let started = Instant::now();
        let (mut value, prepared) = observe(
            &mut self.session,
            &self.page,
            &self.port,
            &args,
            epoch,
            deadline(),
        )
        .await
        .unwrap();
        let commands = self.metrics()["commands"].as_u64().unwrap() - before;
        if let Some(prepared) = prepared {
            self.session.presentation.publish(prepared).unwrap();
        }
        let elapsed = started.elapsed().as_secs_f64() * 1000.;
        let mut oracle = self.session.presentation.oracle.take();
        for result in std::iter::once(&mut value).chain(oracle.iter_mut()) {
            result["session_id"] = self.id.clone().into();
            result["control_epoch"] = epoch.into();
            result["selection"] =
                json!({"version":2,"backend":self.backend,"observation_profile":"semantic_dom_v1"});
        }
        assert!(!value.to_string().contains("PRIVATE-NATIVE-DELTA-CANARY"));
        for baseline in &self.session.presentation.baselines {
            assert!(
                !String::from_utf8_lossy(&baseline.encoded.bytes)
                    .contains("PRIVATE-NATIVE-DELTA-CANARY")
            );
        }
        args["device_id"] = "test".into();
        let mut entry = json!({"commands":commands,"elapsed_ms":elapsed,"cache":self.session.presentation.metrics()});
        entry["args"] = args;
        entry["agent"] = value;
        entry["ordinary"] = oracle.unwrap_or(Value::Null);
        entry
    }
    async fn close(mut self) -> Value {
        self.session.close().await.unwrap();
        let cache = self.session.presentation.metrics();
        assert_eq!(cache["entries"], 0);
        assert_eq!(cache["allocated_bytes"], 0);
        assert_eq!(cache["node_indices"], 0);
        assert_eq!(cache["frame_slots"], 0);
        let native = self.metrics();
        assert_eq!(native["pending"], 0);
        if self.backend == "cdp" {
            assert_eq!(native["native_objects"], 0);
            assert_eq!(native["streams"], 0);
        } else {
            assert_eq!(native["handle_inventory"]["root_handles_returned"], 0);
        }
        assert_eq!(fs::read_dir(&self.directory).unwrap().count(), 0);
        fs::remove_dir(&self.directory).unwrap();
        json!({"cache":cache,"native":native,"directory_removed":true})
    }
}

async fn corpus(backend: &'static str) {
    let workload: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/observation-delta-v1/workload.json"
    ))
    .unwrap();
    assert_eq!(workload["sequences"], 20);
    assert_eq!(workload["updates_per_sequence"], 20);
    let destination = PathBuf::from(std::env::var_os("RDH_DELTA_EVIDENCE").unwrap());
    fs::create_dir_all(&destination).unwrap();
    let mut output =
        fs::File::create_new(destination.join(format!("{backend}-corpus.jsonl"))).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|| async {
            axum::response::Html(include_str!(
                "../../../../tests/fixtures/observation-delta-v1/page.html"
            ))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut fixture = Fixture::new(backend).await;
    let outcome = std::panic::AssertUnwindSafe(async {
        let mut observations = 0;
    let mut summaries = Vec::new();
    for sequence in 0..20 {
        fixture.navigate(&format!("{origin}?sequence={sequence}")).await;
        // Warm identity state, then measure an ordinary observation without a
        // display cache. Both paths retain the same normal observation limits.
        fixture.read(None, false, false).await;
        let baseline = fixture.read(None, false, false).await;
        let commands = baseline["commands"].as_u64().unwrap();
        let mut previous: Option<Value> = None;
        let mut revisions = None;
        let mut changes = Vec::new();
        for step in 0..=20 {
            if step != 0 { fixture.fixture(&format!("window.deltaAdvance({step})")).await; }
            let mut entry = fixture.read(revisions.as_deref(), true, true).await;
            assert_eq!(entry["agent"]["presentation"]["cacheable"], true, "{backend} sequence {sequence} step {step}: {}", entry["agent"]["coverage"]);
            assert!(entry["ordinary"].is_object());
            assert_eq!(entry["commands"].as_u64().unwrap(), commands, "projection must not add native protocol commands");
            let nodes = entry["agent"]["presentation"]["nodes"].as_array().unwrap();
            let changed = if let Some(previous) = &previous {
                let old = previous.as_array().unwrap();
                assert_eq!(old.len(), nodes.len());
                old.iter().zip(nodes).filter(|(a,b)| a != b).count()
            } else { 0 };
            if step != 0 { assert!(changed * 100 <= nodes.len() * 5, "sparse fixture changed {changed}/{} nodes", nodes.len()); }
            changes.push(changed);
            previous = Some(entry["agent"]["presentation"]["nodes"].clone());
            revisions = Some(entry["agent"]["presentation"]["revision"].as_str().unwrap().to_owned());
            entry["sequence_index"] = sequence.into(); entry["step"] = step.into();
            entry["changed_nodes"] = changed.into(); entry["baseline_commands"] = commands.into();
            writeln!(output,"{entry}").unwrap(); output.flush().unwrap();
            observations += 1;
        }
        summaries.push(json!({"sequence":sequence,"ordinary_commands":commands,"changes":changes,"cache":fixture.session.presentation.metrics()}));
        println!("{backend} delta sequence {sequence}: 21 observations, commands={commands}");
    }
        assert_eq!(observations, 420);
        summaries
    }).catch_unwind().await;
    let closed = fixture.close().await;
    server.abort();
    let summaries = match outcome {
        Ok(summaries) => summaries,
        Err(error) => {
            fs::write(
                destination.join(format!("{backend}-failure-cleanup.json")),
                serde_json::to_vec_pretty(&closed).unwrap(),
            )
            .unwrap();
            std::panic::resume_unwind(error);
        }
    };
    fs::write(destination.join(format!("{backend}-summary.json")), serde_json::to_vec_pretty(&json!({"backend":backend,"workload":workload,"same_read_oracles":420,"summaries":summaries,"closed":closed})).unwrap()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit native Chrome, Firefox helper and same-read evidence directory"]
async fn native_delta_corpus_cdp() {
    corpus("cdp").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit native Chrome, Firefox helper and same-read evidence directory"]
async fn native_delta_corpus_bidi() {
    corpus("bidi").await;
}

#[path = "presentation_resource_tests.rs"]
mod resources;
