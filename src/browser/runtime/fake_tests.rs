use super::*;
use crate::browser::engine::{Chunk, Request, Stream, TargetInfo, testing::Ids};

#[derive(Clone, Default)]
struct Fake {
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl SessionPort for Fake {
    fn connected(&self) -> bool {
        true
    }
    fn stop(&self) {
        self.calls.lock().unwrap().push("stop");
    }
    async fn initialize_context(&self, _: &Context, _: bool, _: bool, _: Instant) -> Result<()> {
        self.calls.lock().unwrap().push("initialize");
        Ok(())
    }
    async fn resume_context(&self, _: &Context, _: Instant) -> Result<()> {
        self.calls.lock().unwrap().push("resume");
        Ok(())
    }
    async fn discard_tab(&self, _: &TabId, _: Instant) -> Result<()> {
        self.calls.lock().unwrap().push("discard");
        Ok(())
    }
    async fn respond(&self, _: &Request, decision: RequestDecision, _: Instant) -> Result<()> {
        self.calls.lock().unwrap().push(match decision {
            RequestDecision::Continue => "continue",
            RequestDecision::Deny => "deny",
            RequestDecision::Abort => "abort",
        });
        Ok(())
    }
}
impl TransferPort for Fake {
    async fn open_stream(&self, _: &Request, _: Instant) -> Result<Stream> {
        panic!("no download in this fixture")
    }
    async fn read_stream(&self, _: &Stream, _: usize, _: Instant) -> Result<Chunk> {
        panic!("no download in this fixture")
    }
    async fn close_stream(&self, _: Stream, _: Instant) -> Result<()> {
        panic!("no download in this fixture")
    }
}
fn attached(ids: Ids, tab: &str, context: &str) -> Event {
    Event::Attached {
        context: ids.context(context),
        info: TargetInfo {
            tab: ids.tab(tab),
            root: ids.frame_id(tab),
            kind: ContextKind::Page,
            url: "about:blank".into(),
            title: String::new(),
        },
    }
}
#[tokio::test]
async fn session_policy_accepts_semantic_events_without_a_native_endpoint() {
    let ids = Ids::new();
    let fake = Fake::default();
    let targets = Arc::new(Mutex::new(HashMap::new()));
    let gate = Arc::new(Gate::default());
    let mut work = Work::default();
    handle_event(
        &fake,
        &targets,
        None,
        &gate,
        &mut work,
        attached(ids, "tab", "page"),
    )
    .await
    .unwrap();
    assert!(!targets.lock().unwrap()["tab"].ready);
    work.initializers.join_next().await.unwrap().unwrap();
    assert!(targets.lock().unwrap()["tab"].ready);
    let context = ids.context("page");
    let paused = |url: &str, responded: bool| {
        Event::DocumentPaused(DocumentRequest {
            handle: ids.request(&context, "request"),
            context: Some(context.clone()),
            frame: ids.frame_id("tab"),
            network: ids.network(&context, "network"),
            url: url.into(),
            responded,
            redirected: false,
            status: None,
            headers: None,
        })
    };
    handle_event(
        &fake,
        &targets,
        None,
        &gate,
        &mut work,
        paused("http://169.254.169.254/", false),
    )
    .await
    .unwrap();
    assert_eq!(targets.lock().unwrap()["tab"].blocked, Some(false));
    handle_event(
        &fake,
        &targets,
        None,
        &gate,
        &mut work,
        paused("http://169.254.169.254/", true),
    )
    .await
    .unwrap();
    assert_eq!(targets.lock().unwrap()["tab"].blocked, Some(true));
    handle_event(
        &fake,
        &targets,
        None,
        &gate,
        &mut work,
        paused("http://127.0.0.1/", false),
    )
    .await
    .unwrap();
    assert_eq!(targets.lock().unwrap()["tab"].blocked, None);
    handle_event(
        &fake,
        &targets,
        None,
        &gate,
        &mut work,
        Event::Detached(context),
    )
    .await
    .unwrap();
    assert!(targets.lock().unwrap().is_empty());
    assert_eq!(
        *fake.calls.lock().unwrap(),
        ["initialize", "deny", "deny", "continue"]
    );
}
#[tokio::test]
async fn semantic_session_limits_reject_one_more_tab_without_an_initializer() {
    let ids = Ids::new();
    let fake = Fake::default();
    let targets = Arc::new(Mutex::new(HashMap::new()));
    let gate = Arc::new(Gate::default());
    let mut work = Work::default();
    for i in 0..=MAX_TABS {
        handle_event(
            &fake,
            &targets,
            None,
            &gate,
            &mut work,
            attached(ids, &format!("tab{i}"), &format!("context{i}")),
        )
        .await
        .unwrap();
        while let Some(done) = work.initializers.join_next().await {
            done.unwrap();
        }
    }
    assert_eq!(targets.lock().unwrap().len(), MAX_TABS);
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "initialize")
            .count(),
        MAX_TABS
    );
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| **c == "discard")
            .count(),
        1
    );
}
#[test]
fn connection_evidence_cache_keeps_its_frozen_bound() {
    let ids = Ids::new();
    let context = ids.context("page");
    let mut cache = Spaces::default();
    for i in 0..1025 {
        cache.record(ids.network(&context, &format!("{i}")), i % 2 == 0);
    }
    assert_eq!((cache.known.len(), cache.order.len()), (1024, 1024));
    assert_eq!(cache.take(&ids.network(&context, "0")), None);
    assert_eq!(cache.take(&ids.network(&context, "1")), Some(false));
    assert_eq!(cache.take(&ids.network(&context, "1024")), Some(true));
}
