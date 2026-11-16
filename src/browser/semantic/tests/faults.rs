use super::{
    observation::{self, Scene},
    *,
};
use tokio::sync::oneshot;
type Gate = (oneshot::Receiver<()>, oneshot::Sender<()>);
pub(super) struct FakeGate {
    pub reached: oneshot::Sender<()>,
    pub release: oneshot::Receiver<()>,
}
pub(super) trait NativeGate: Port {
    fn gate(&self) -> Gate;
}
impl NativeGate for engine::semantic_cdp::Semantic {
    fn gate(&self) -> Gate {
        self.hold_next_reply()
    }
}
impl NativeGate for engine::bidi::page::Semantic {
    fn gate(&self) -> Gate {
        self.hold_next_reply()
    }
}
impl NativeGate for Fake {
    fn gate(&self) -> Gate {
        let (reached, ready) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let mut gate = self.reply_gate.lock().unwrap();
        assert!(gate.is_none());
        *gate = Some(FakeGate {
            reached,
            release: wait,
        });
        (ready, release)
    }
}

#[derive(Clone)]
struct Controlled<P> {
    port: P,
    armed: Arc<Mutex<Option<oneshot::Sender<Gate>>>>,
}
impl<P: NativeGate> Controlled<P> {
    fn new(port: P) -> Self {
        Self {
            port,
            armed: Arc::new(Mutex::new(None)),
        }
    }
    fn arm(&self) -> oneshot::Receiver<Gate> {
        let (sender, receiver) = oneshot::channel();
        let mut armed = self.armed.lock().unwrap();
        assert!(armed.is_none());
        *armed = Some(sender);
        receiver
    }
}
impl<P: NativeGate> Port for Controlled<P> {
    fn coordination(&self) -> Arc<Coordination> {
        self.port.coordination()
    }
    fn authorize(&self, control: Arc<Control>, epoch: u64, human: bool) -> Self {
        Self {
            port: self.port.authorize(control, epoch, human),
            ..self.clone()
        }
    }
    fn tracked(&self) -> (Self, Arc<AtomicBool>) {
        let (port, flag) = self.port.tracked();
        (
            Self {
                port,
                ..self.clone()
            },
            flag,
        )
    }
    fn connected(&self) -> bool {
        self.port.connected()
    }
    fn stop(&self) {
        self.port.stop()
    }
    fn pending(&self) -> usize {
        self.port.pending()
    }
    async fn frames(&self, deadline: Instant) -> Result<Vec<Frame>> {
        self.port.frames(deadline).await
    }
    async fn refresh(&self, frame: &Frame, deadline: Instant) -> Result<Frame> {
        self.port.refresh(frame, deadline).await
    }
    async fn target(&self, frame: &Frame, node: u64, deadline: Instant) -> Result<Geometry> {
        self.port.target(frame, node, deadline).await
    }
    async fn program(&self, frame: &Frame, program: Program, deadline: Instant) -> Result<Output> {
        self.port.program(frame, program, deadline).await
    }
    async fn input(&self, frame: &Frame, input: NativeInput, deadline: Instant) -> Result<()> {
        if let Some(sender) = self.armed.lock().unwrap().take() {
            let _ = sender.send(self.port.gate());
        }
        self.port.input(frame, input, deadline).await
    }
}
#[derive(Clone, Copy, Debug)]
enum Mode {
    Control,
    Cancel,
    Timeout,
}
pub(super) async fn assert_owned_inputs<P: NativeGate + observation::Harness>(
    port: P,
    label: &str,
) -> Value {
    port.scene(Scene::Controls).await.unwrap();
    let controlled = Controlled::new(port.clone());
    let page = Page::new(controlled.clone());
    let mut cases = Vec::new();
    for mode in [Mode::Control, Mode::Cancel, Mode::Timeout] {
        for iteration in 0..20 {
            let observed = observation::read(&page).await;
            let target = observation::reference(&observed, "keys");
            port.clear().await.unwrap();
            let armed = controlled.arm();
            let owner = page.clone();
            let action_deadline = if matches!(mode, Mode::Timeout) {
                Instant::now() + Duration::from_millis(750)
            } else {
                deadline()
            };
            let mut task = Some(tokio::spawn(async move {
                owner
                    .act(target, Action::Press(Key::ArrowRight), action_deadline)
                    .await
            }));
            let (ready, release) = tokio::time::timeout(Duration::from_secs(5), armed)
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), ready)
                .await
                .unwrap()
                .unwrap();
            assert!(page.resources().1 > 0);
            assert!(
                page.resources().2 > 0,
                "native input receipt must remain owned"
            );
            match mode {
                Mode::Control => {
                    page.pause();
                }
                Mode::Cancel => {
                    let cancelled = task.take().unwrap();
                    cancelled.abort();
                    assert!(cancelled.await.unwrap_err().is_cancelled());
                    assert!(page.control().snapshot().1);
                }
                Mode::Timeout => {
                    tokio::time::sleep_until((action_deadline + Duration::from_millis(15)).into())
                        .await;
                }
            }
            let error = page
                .claim_human(Instant::now() + Duration::from_millis(10))
                .await
                .err()
                .expect("pending input cannot be handed off");
            assert_eq!(error.code, "browser_cleanup_pending");
            release
                .send(())
                .expect("native receipt still owned at release");
            let result = if let Some(task) = task {
                let error = task.await.unwrap().unwrap_err();
                let result = json!({"code":error.code,"state":error.state});
                assert_eq!(error.state, "unknown");
                result
            } else {
                json!({"caller":"cancelled"})
            };
            page.settled(deadline()).await.unwrap();
            assert_eq!((page.resources().1, page.resources().2), (0, 0));
            assert!(page.control().snapshot().1);
            let evidence = port.evidence("keys").await.unwrap();
            let item = json!({"mode":format!("{mode:?}"),"iteration":iteration,"result":result,"evidence":evidence,"input_tasks":page.resources().1,"pending_effects":page.resources().2});
            if let Some(root) = std::env::var_os("RDH_D3_EVIDENCE") {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(PathBuf::from(root).join(format!("{label}-ownership-trace.jsonl")))
                    .unwrap();
                writeln!(file, "{item}").unwrap();
            }
            let events = evidence["events"].as_array().unwrap();
            for kind in ["keydown", "keyup"] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event["type"] == kind && event["key"] == "ArrowRight")
                        .count(),
                    1,
                    "{item}"
                );
            }
            cases.push(item);
            page.resume(page.control().snapshot().0, true, deadline())
                .await
                .unwrap();
            let target = observation::reference(&observation::read(&page).await, "keys");
            port.clear().await.unwrap();
            page.act(target, Action::Press(Key::ArrowLeft), deadline())
                .await
                .unwrap();
            let followup = port.evidence("keys").await.unwrap();
            assert!(
                followup["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|event| event["type"] == "keydown")
                    .all(|event| event["ctrl"] == false && event["meta"] == false)
            );
        }
    }
    json!({"control_cases":20,"caller_cancellation_cases":20,"deadline_cases":20,"replayed_inputs":0,"cleanup_before_handoff":true,"cases":cases})
}
#[tokio::test]
async fn fake_uses_shared_unknown_cancellation_and_owned_release_assertions() {
    assert_owned_inputs(Fake::new(), "fake").await;
}

pub(super) async fn assert_connection_lost<P: NativeGate + observation::Harness>(port: P) -> Value {
    port.scene(Scene::Controls).await.unwrap();
    let controlled = Controlled::new(port.clone());
    let page = Page::new(controlled.clone());
    let target = observation::reference(&observation::read(&page).await, "keys");
    let armed = controlled.arm();
    let owner = page.clone();
    let task = tokio::spawn(async move {
        owner
            .act(target, Action::Press(Key::ArrowRight), deadline())
            .await
    });
    let (ready, release) = tokio::time::timeout(Duration::from_secs(5), armed)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    assert!(page.resources().2 > 0);
    port.stop();
    let _ = release.send(());
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.state, "unknown");
    assert!(page.control().snapshot().1);
    assert!(page.claim_human(deadline()).await.is_err());
    assert!(
        page.resume(page.control().snapshot().0, true, deadline())
            .await
            .is_err()
    );
    assert_eq!((page.resources().1, page.resources().2), (0, 0));
    json!({"result":{"code":error.code,"state":error.state},"human_permit_refused":true,"replay":0,"input_tasks":0,"pending_effects":0})
}
#[tokio::test]
async fn fake_lost_connection_never_grants_human_input() {
    assert_connection_lost(Fake::new()).await;
}
