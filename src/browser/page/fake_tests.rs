//! The real page algorithm exercised with semantic data, without a protocol
//! endpoint, command name, native JSON envelope or native ID constructor.
use super::*;
use crate::browser::engine::{self, Accessible, Batch, Hit, ReaderStatus, Viewport, testing::Ids};
use std::sync::{Arc, Mutex};

struct State {
    frame: Frame,
    details: Value,
    closed: bool,
    opened: usize,
    released: usize,
    effects: usize,
    changed: bool,
    asked: Option<ReaderOptions>,
}
struct Fake {
    ids: Ids,
    state: Arc<Mutex<State>>,
}
impl Fake {
    fn new() -> Self {
        let ids = Ids::new();
        Self {
            ids,
            state: Arc::new(Mutex::new(State {
                frame: ids.frame("context", "top", "first", "https://fixture.invalid/"),
                details: json!({"connected":true,"signature":"record one","tag":"BUTTON","type":"submit","enabled":true,"visible":true,"steady":true,"hit":true,"in_view":true,"covered":false,"inner":false}),
                closed: true,
                opened: 0,
                released: 0,
                effects: 0,
                changed: false,
                asked: None,
            })),
        }
    }
    fn frame(&self) -> Frame {
        self.state.lock().unwrap().frame.clone()
    }
    fn node(&self) -> Node {
        self.ids.node(&self.frame().session, 1)
    }
}
impl PageReadPort for Fake {
    async fn frames(&self, _: &Context, _: Instant) -> Result<Vec<Frame>> {
        Ok(vec![self.frame()])
    }
    async fn metadata(&self, _: &Frame, _: Instant) -> Result<Value> {
        Ok(json!({"url":self.frame().url,"title":"Fixture","ready_state":"complete"}))
    }
    async fn open_reader(&self, frame: &Frame, options: ReaderOptions, _: Instant) -> Result<Scan> {
        let mut state = self.state.lock().unwrap();
        state.closed = false;
        state.opened += 1;
        state.asked = Some(options);
        Ok(self.ids.scan(&frame.session, "scan"))
    }
    async fn reader_status(&self, _: &Scan, _: Instant) -> Result<ReaderStatus> {
        let s = self.state.lock().unwrap();
        Ok(ReaderStatus {
            closed: s.closed,
            changed: s.changed,
            more: false,
        })
    }
    async fn collect(&self, _: &Scan, _: usize, _: usize, _: Instant) -> Result<Batch> {
        Ok(Batch {
            nodes: vec![json!({"index":0,"kind":"native"})],
            changed: false,
            scanned: 1,
            matches: 0,
            omitted: Vec::new(),
        })
    }
    async fn ack(
        &self,
        scan: &Scan,
        count: usize,
        chars: usize,
        d: Instant,
    ) -> Result<ReaderStatus> {
        assert_eq!((count, chars), (1, 0));
        self.reader_status(scan, d).await
    }
    async fn close_reader(&self, _: &Scan, _: Instant) {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        s.released += 1;
    }
    async fn release_scratch(&self, _: &Context, _: Instant) {}
    async fn locate(
        &self,
        _: &Frame,
        _: &Scan,
        index: u64,
        _: Instant,
    ) -> Result<Option<engine::Found>> {
        assert_eq!(index, 0);
        Ok(Some(engine::Found {
            node: self.node(),
            exposed: true,
            role: "button".into(),
            name: "Save".into(),
            value: Value::Null,
            data: None,
        }))
    }
    async fn details_of(&self, _: &Scan, indexes: &[u64], _: Instant) -> Result<Value> {
        Ok(indexes
            .iter()
            .map(|_| self.state.lock().unwrap().details.clone())
            .collect())
    }
}
impl PageActionPort for Fake {
    async fn details(&self, _: &Frame, _: &Node, _: Instant) -> Result<Value> {
        Ok(self.state.lock().unwrap().details.clone())
    }
    async fn accessibility(&self, _: &Node, _: Instant) -> Result<Vec<Accessible>> {
        Ok(vec![Accessible {
            exposed: true,
            role: "button".into(),
            name: "Save".into(),
            value: Value::Null,
        }])
    }
    async fn layout(&self, _: &Context, _: Instant) -> Result<Viewport> {
        panic!("top-level click does not query ancestor geometry")
    }
    async fn frame_owner(&self, _: &Context, _: &engine::FrameId, _: Instant) -> Result<Node> {
        panic!("no ancestor in fixture")
    }
    async fn quad(&self, _: &Node, _: Instant) -> Result<[f64; 8]> {
        Ok([0., 0., 20., 0., 20., 20., 0., 20.])
    }
    async fn hit_at(&self, _: &Context, point: Point, _: Instant) -> Result<Hit> {
        assert_eq!(point, Point { x: 10., y: 10. });
        Ok(Hit {
            frame: self.frame().id,
            node: self.node(),
        })
    }
    async fn scroll(&self, _: &Frame, _: f64, _: Instant) -> Result<Point> {
        panic!("not requested")
    }
    async fn scroll_into_view(&self, _: &Node, _: Instant) -> Result<()> {
        panic!("already in view")
    }
    async fn focus(&self, _: &Node, _: Instant) -> Result<()> {
        panic!("click does not focus explicitly")
    }
    async fn click(&self, _: &Context, _: Point, _: Instant) -> Result<()> {
        self.state.lock().unwrap().effects += 1;
        Ok(())
    }
    async fn select_all(&self, _: &Context, _: Instant) -> Result<()> {
        panic!("not requested")
    }
    async fn press(&self, _: &Context, _: &str, _: Instant) -> Result<()> {
        panic!("not requested")
    }
    async fn insert_text(&self, _: &Context, _: &str, _: Instant) -> Result<()> {
        panic!("not requested")
    }
    async fn select(&self, _: &Frame, _: &Node, _: &str, _: Instant) -> Result<bool> {
        panic!("not requested")
    }
    async fn set_form_value(
        &self,
        _: &Frame,
        _: &Node,
        _: &str,
        _: bool,
        _: Instant,
    ) -> Result<Option<bool>> {
        panic!("not requested")
    }
    async fn attach_file(&self, _: &Node, _: &std::path::Path, _: Instant) -> Result<()> {
        panic!("not requested")
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

#[tokio::test]
async fn public_observation_and_action_planning_run_on_semantic_ports() {
    let fake = Fake::new();
    let (value, refs) = observe(&fake, &[fake.frame()], 10, 1000, deadline())
        .await
        .unwrap();
    assert_eq!(value["nodes"][0]["name"], "Save");
    assert_eq!(value["nodes"][0]["frame_id"], "top");
    assert_eq!(value["coverage"]["complete_within_scope"], true);
    let element = refs.values().next().unwrap();
    let error = element
        .act(
            &fake,
            &json!({"kind":"fill","text":"wrong kind"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_invalid_params", "not_sent")
    );
    assert_eq!(fake.state.lock().unwrap().effects, 0);
    element
        .act(&fake, &json!({"kind":"click"}), deadline())
        .await
        .unwrap();
    assert_eq!(fake.state.lock().unwrap().effects, 1);
    fake.state.lock().unwrap().details["signature"] = "record two".into();
    let error = element
        .act(&fake, &json!({"kind":"click"}), deadline())
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_stale_reference", "not_sent")
    );
    assert_eq!(fake.state.lock().unwrap().effects, 1);
    let s = fake.state.lock().unwrap();
    assert_eq!((s.opened, s.released), (1, 1));
    assert!(!s.asked.as_ref().unwrap().interactive);
}

#[tokio::test]
async fn replacement_and_changed_coverage_do_not_revive_old_references() {
    let fake = Fake::new();
    let (_, old) = observe(&fake, &[fake.frame()], 10, 1000, deadline())
        .await
        .unwrap();
    let frame = fake
        .ids
        .frame("context", "top", "second", "https://fixture.invalid/");
    fake.state.lock().unwrap().frame = frame;
    fake.state.lock().unwrap().changed = true;
    let (value, new) = observe(&fake, &[fake.frame()], 10, 1000, deadline())
        .await
        .unwrap();
    assert_eq!(value["coverage"]["changed_during_read"], true);
    assert_eq!(value["coverage"]["complete_within_scope"], false);
    assert_ne!(old.keys().next(), new.keys().next());
    let error = old
        .values()
        .next()
        .unwrap()
        .act(&fake, &json!({"kind":"click"}), deadline())
        .await
        .unwrap_err();
    assert_eq!(error.code, "browser_stale_reference");
    assert_eq!(fake.state.lock().unwrap().effects, 0);
}

#[tokio::test]
async fn frame_budget_accepts_the_limit_and_refuses_one_more_before_reading() {
    let fake = Fake::new();
    let frames = vec![fake.frame(); 32];
    let cursor = Cursor::new(&frames, Scope::default()).unwrap();
    cursor.close(&fake, deadline()).await;
    assert_eq!(fake.state.lock().unwrap().opened, 0);
    let error = Cursor::new(&vec![fake.frame(); 33], Scope::default())
        .err()
        .unwrap();
    assert_eq!(
        (error.code, error.state),
        ("browser_frame_limit", "not_sent")
    );
    assert_eq!(fake.state.lock().unwrap().opened, 0);
}
