use super::program::{Output, Program};
use super::*;
use crate::browser::{
    control::{Effect, Submission},
    engine,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
mod cleanup;
mod controls;
mod faults;
mod frames;
mod identity;
mod input;
mod navigation;
mod observation;
#[cfg(target_os = "linux")]
mod performance;
mod resources;

#[derive(Deserialize)]
struct Oracle {
    names: Vec<NameCase>,
    replace_text: Vec<String>,
}
#[derive(Deserialize)]
struct NameCase {
    id: String,
    html: String,
    role: String,
    name: String,
    source: String,
    quality: Vec<String>,
    actionable: bool,
}
fn oracle() -> Oracle {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/semantic-dom-v1/oracle.json"
    ))
    .unwrap()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

async fn names<P: Port>(port: P) -> Value {
    let page = Page::new(port);
    let observed = page
        .observe(Query::default(), 200, 64_000, None, deadline())
        .await
        .unwrap();
    assert!(observed.complete, "incomplete naming fixture");
    let mut result = Vec::new();
    for case in oracle().names {
        let rows = observed
            .rows
            .iter()
            .filter(|row| row.facts.id == case.id)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "fixture {}", case.id);
        let row = rows[0];
        assert_eq!(
            (
                &row.facts.role,
                &row.facts.name,
                &row.facts.name_source,
                &row.facts.name_quality,
                row.reference.is_some()
            ),
            (
                &case.role,
                &case.name,
                &case.source,
                &case.quality,
                case.actionable
            ),
            "fixture {}",
            case.id
        );
        result.push(json!({"id":case.id,"role":row.facts.role,"name":row.facts.name,"source":row.facts.name_source,"quality":row.facts.name_quality,"actionable":row.reference.is_some()}));
    }
    json!({"profile":observed.profile,"cases":result,"complete":observed.complete})
}

#[derive(Clone)]
struct Fake {
    data: Arc<Mutex<FakeData>>,
    submission: Submission,
    effects: Arc<AtomicUsize>,
    coordination: Arc<Coordination>,
    reply_gate: Arc<Mutex<Option<faults::FakeGate>>>,
    pending: Arc<AtomicUsize>,
}
struct FakeData {
    frame: Frame,
    rows: Vec<Facts>,
    children: Vec<(Frame, Vec<Facts>)>,
    filtered: Vec<u64>,
    leased: Vec<u64>,
    position: usize,
    sequence: u64,
    alive: bool,
    focused: Option<u64>,
    events: Vec<Value>,
}
impl Fake {
    fn new() -> Self {
        let mut identities = Identities::new();
        let marker = Marker {
            realm: "fixture-realm".into(),
            restoration: 0,
            url: "http://fixture/".into(),
        };
        let frame = identities
            .observe("fixture", None, "document", "realm", marker)
            .unwrap();
        let mut rows: Vec<Facts> = oracle()
            .names
            .into_iter()
            .enumerate()
            .map(|(index, case)| {
                let node = index as u64 + 1;
                Facts {
                    node,
                    id: case.id.clone(),
                    role: case.role,
                    kind: if case.id == "inferred" {
                        "inferred"
                    } else if matches!(case.id.as_str(), "forbidden" | "explicit") {
                        "aria"
                    } else {
                        "native"
                    }
                    .into(),
                    name: case.name,
                    name_source: case.source,
                    name_quality: case.quality,
                    value: matches!(case.id.as_str(), "label" | "editable").then(String::new),
                    enabled: case.id != "disabled",
                    readonly: case.id == "readonly",
                    editable: matches!(case.id.as_str(), "label" | "aria" | "title" | "editable"),
                    sensitive: false,
                    checked: case.id == "explicit",
                    tag: if case.id == "label" {
                        "input"
                    } else if case.id == "editable" {
                        "div"
                    } else {
                        "button"
                    }
                    .into(),
                    input_type: String::new(),
                    visible: true,
                    hit: true,
                    x: 10.,
                    y: 10.,
                    in_view: true,
                    record: Material {
                        entries: vec![Entry {
                            identity: node,
                            tag: "button".into(),
                            attributes: vec![],
                            text: case.id,
                            value: None,
                            checked: false,
                        }],
                        complete: true,
                        limits: vec![],
                    },
                }
            })
            .collect();
        let mut multiline = rows.iter().find(|row| row.id == "label").unwrap().clone();
        multiline.node = 100;
        multiline.id = "multiline".into();
        multiline.tag = "textarea".into();
        multiline.name = "Multiline".into();
        multiline.record.entries[0].identity = 100;
        rows.push(multiline);
        Self {
            data: Arc::new(Mutex::new(FakeData {
                frame,
                rows,
                children: Vec::new(),
                filtered: Vec::new(),
                leased: Vec::new(),
                position: 0,
                sequence: 0,
                alive: true,
                focused: None,
                events: Vec::new(),
            })),
            submission: Submission::default(),
            effects: Arc::new(AtomicUsize::new(0)),
            coordination: Arc::new(Coordination::new()),
            reply_gate: Arc::new(Mutex::new(None)),
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl FakeData {
    fn frame(&self, context: Context) -> Result<&Frame> {
        if self.frame.context == context {
            Ok(&self.frame)
        } else {
            self.children
                .iter()
                .find(|(frame, _)| frame.context == context)
                .map(|(frame, _)| frame)
                .ok_or_else(types::stale)
        }
    }
    fn rows(&self, context: Context) -> Result<&Vec<Facts>> {
        if self.frame.context == context {
            Ok(&self.rows)
        } else {
            self.children
                .iter()
                .find(|(frame, _)| frame.context == context)
                .map(|(_, rows)| rows)
                .ok_or_else(types::stale)
        }
    }
    fn rows_mut(&mut self, context: Context) -> Result<&mut Vec<Facts>> {
        if self.frame.context == context {
            Ok(&mut self.rows)
        } else {
            self.children
                .iter_mut()
                .find(|(frame, _)| frame.context == context)
                .map(|(_, rows)| rows)
                .ok_or_else(types::stale)
        }
    }
}
impl Port for Fake {
    fn coordination(&self) -> Arc<Coordination> {
        self.coordination.clone()
    }
    fn authorize(&self, control: Arc<Control>, epoch: u64, human: bool) -> Self {
        Self {
            submission: self.submission.authorized(control, epoch, human),
            ..self.clone()
        }
    }
    fn tracked(&self) -> (Self, Arc<AtomicBool>) {
        let (submission, flag) = self.submission.track_effects();
        (
            Self {
                submission,
                ..self.clone()
            },
            flag,
        )
    }
    fn connected(&self) -> bool {
        self.data.lock().unwrap().alive
    }
    fn stop(&self) {
        self.data.lock().unwrap().alive = false;
    }
    fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
    async fn frames(&self, _: Instant) -> Result<Vec<Frame>> {
        self.submission.enqueue(Effect::None, || {
            let data = self.data.lock().unwrap();
            Ok(std::iter::once(data.frame.clone())
                .chain(data.children.iter().map(|(frame, _)| frame.clone()))
                .collect())
        })
    }
    async fn refresh(&self, frame: &Frame, _: Instant) -> Result<Frame> {
        self.submission.enqueue(Effect::None, || {
            let data = self.data.lock().unwrap();
            Ok(data.frame(frame.context)?.clone())
        })
    }
    async fn target(&self, frame: &Frame, node: u64, _: Instant) -> Result<Geometry> {
        self.submission.enqueue(Effect::None, || {
            let data = self.data.lock().unwrap();
            let row = data
                .rows(frame.context)?
                .iter()
                .find(|row| row.node == node)
                .ok_or_else(types::stale)?;
            Ok(Geometry {
                x: row.x,
                y: row.y,
                width: 1024.,
                height: 768.,
                visible: row.visible,
                hit: row.hit,
                in_view: row.in_view,
            })
        })
    }
    async fn program(&self, frame: &Frame, program: Program, _: Instant) -> Result<Output> {
        self.submission.enqueue(program.effect(), || {
            let mut data = self.data.lock().unwrap();
            match program {
                Program::Stamp => Ok(Output::Marker(data.frame(frame.context)?.marker.clone())),
                Program::Open(query) => {
                    data.position = 0;
                    data.sequence = 0;
                    data.filtered = data
                        .rows(frame.context)?
                        .iter()
                        .filter(|row| {
                            row.visible
                                && (query.filter != Filter::Interactive
                                    || types::interactive(&row.role)
                                    || row.role == "inferred"
                                    || row.role == "heading")
                                && (query.filter != Filter::Viewport || row.in_view)
                                && query.find.as_ref().is_none_or(|needle| {
                                    row.name.contains(needle)
                                        || row.record.entries.iter().any(|entry| {
                                            entry.text.contains(needle)
                                                || entry
                                                    .value
                                                    .as_ref()
                                                    .is_some_and(|value| value.contains(needle))
                                        })
                                })
                        })
                        .map(|row| row.node)
                        .collect();
                    Ok(Output::Marker(data.frame(frame.context)?.marker.clone()))
                }
                Program::Next {
                    sequence, nodes, ..
                } => {
                    if sequence != data.sequence {
                        return Err(types::stale());
                    }
                    let end = (data.position + nodes).min(data.filtered.len());
                    let rows: Vec<Facts> = data.filtered[data.position..end]
                        .iter()
                        .map(|node| {
                            data.rows(frame.context)
                                .unwrap()
                                .iter()
                                .find(|row| row.node == *node)
                                .unwrap()
                                .clone()
                        })
                        .collect();
                    data.leased = rows.iter().map(|row| row.node).collect();
                    data.position = end;
                    data.sequence += 1;
                    Ok(Output::Batch(Batch {
                        stamp: data.frame(frame.context)?.marker.clone(),
                        rows,
                        sequence: data.sequence,
                        more: end < data.filtered.len(),
                        scanned: end,
                        changed: false,
                        omitted: vec![
                            "closed_shadow_not_observable".into(),
                            "canvas_pixels_outside_profile".into(),
                            "browser_ui_outside_profile".into(),
                        ],
                        characters: 100,
                    }))
                }
                Program::Inspect { node, .. } => Ok(Output::Facts(
                    data.rows(frame.context)?
                        .iter()
                        .find(|row| row.node == node)
                        .cloned()
                        .map(Box::new),
                )),
                Program::Recheck { nodes, .. } => Ok(Output::Checks(
                    nodes
                        .into_iter()
                        .map(|node| {
                            data.rows(frame.context)
                                .unwrap()
                                .iter()
                                .find(|row| row.node == node)
                                .cloned()
                        })
                        .collect(),
                )),
                Program::Release => {
                    data.position = 0;
                    data.filtered.clear();
                    data.leased.clear();
                    Ok(Output::Applied(Some(true)))
                }
                Program::ClearNodes { .. } => {
                    data.leased.clear();
                    Ok(Output::Applied(Some(true)))
                }
                Program::Stats => Ok(Output::Stats(RegistryStats {
                    nodes: data.leased.len(),
                    scans: usize::from(data.position < data.filtered.len()),
                    pending: data.filtered.len().saturating_sub(data.position),
                })),
                Program::Focus { node, .. } => {
                    data.focused = Some(node);
                    self.effects.fetch_add(1, Ordering::AcqRel);
                    Ok(Output::Applied(Some(true)))
                }
                Program::Scroll { node, .. } => {
                    let row = data
                        .rows_mut(frame.context)?
                        .iter_mut()
                        .find(|row| row.node == node)
                        .ok_or_else(types::stale)?;
                    row.in_view = true;
                    row.hit = true;
                    self.effects.fetch_add(1, Ordering::AcqRel);
                    Ok(Output::Applied(Some(true)))
                }
                Program::FormCheck { node, value, .. } => {
                    let row = data
                        .rows(frame.context)?
                        .iter()
                        .find(|row| row.node == node)
                        .ok_or_else(types::stale)?;
                    Ok(Output::Applied(
                        (row.id != "select" || matches!(value.as_str(), "a" | "b")).then_some(true),
                    ))
                }
                Program::Form { node, value, .. } => {
                    let row = data
                        .rows_mut(frame.context)?
                        .iter_mut()
                        .find(|row| row.node == node)
                        .ok_or_else(types::stale)?;
                    let changed = row.value.as_ref() != Some(&value);
                    row.value = Some(value);
                    let id = row.id.clone();
                    if changed {
                        self.effects.fetch_add(1, Ordering::AcqRel);
                        for kind in ["input", "change"] {
                            data.events
                                .push(json!({"type":kind,"id":id,"trusted":false}));
                        }
                    }
                    Ok(Output::Applied(Some(changed)))
                }
                _ => {
                    self.effects.fetch_add(1, Ordering::AcqRel);
                    Ok(Output::Applied(Some(true)))
                }
            }
        })
    }
    async fn input(&self, frame: &Frame, input: NativeInput, deadline: Instant) -> Result<()> {
        let pressed = match &input {
            NativeInput::Press(key) => Some(format!("{key:?}")),
            _ => None,
        };
        self.submission.enqueue(Effect::Page, || {
            self.effects.fetch_add(1, Ordering::AcqRel);
            let mut data=self.data.lock().unwrap();
            match input {
                NativeInput::Click{node,..}=>{
                    let row=data.rows_mut(frame.context)?.iter_mut().find(|row|row.node==node).unwrap();if matches!(row.role.as_str(),"checkbox"|"radio"|"switch"){row.checked = !row.checked;}let id=row.id.clone();
                    data.events.push(json!({"type":"click","id":id,"trusted":true}));
                },
                NativeInput::ReplaceText(value)=>{
                    let focused=data.focused.unwrap();let row=data.rows_mut(frame.context)?.iter_mut().find(|row|row.node==focused).unwrap();
                    row.value=Some(value.clone());let id=row.id.clone();
                    data.events.push(json!({"type":"beforeinput","id":id,"trusted":true,"data":value}));
                    data.events.push(json!({"type":"input","id":id,"trusted":true,"data":value}));
                },
                NativeInput::Press(key)=>{
                    let id=data.rows(frame.context)?.iter().find(|row|Some(row.node)==data.focused).unwrap().id.clone();let key=format!("{key:?}");
                    data.events.push(json!({"type":"keydown","id":id,"key":key,"trusted":true,"ctrl":false,"meta":false}));
                }
            }
            Ok(())
        })?;
        let gate = self.reply_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            self.pending.fetch_add(1, Ordering::AcqRel);
            let _ = gate.reached.send(());
            let _ = gate.release.await;
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
        if !self.connected() {
            return Err(crate::browser::Failure::unknown("browser_lost"));
        }
        if let Some(key) = pressed {
            let mut data = self.data.lock().unwrap();
            let id = data
                .rows(frame.context)?
                .iter()
                .find(|row| Some(row.node) == data.focused)
                .unwrap()
                .id
                .clone();
            data.events.push(
                json!({"type":"keyup","id":id,"key":key,"trusted":true,"ctrl":false,"meta":false}),
            );
        }
        if Instant::now() >= deadline {
            return Err(crate::browser::Failure::unknown("browser_timeout_unknown"));
        }
        self.submission.deliver(())
    }
}

#[tokio::test]
async fn fake_uses_the_frozen_naming_and_reference_assertions() {
    names(Fake::new()).await;
}

#[tokio::test]
async fn reference_material_identity_paging_and_control_are_common() {
    let fake = Fake::new();
    let page = Page::new(fake.clone());
    let read = page
        .observe(Query::default(), 1, 64_000, None, deadline())
        .await
        .unwrap();
    let first = read.rows[0].reference.clone().unwrap();
    fake.data.lock().unwrap().rows[1].name = "unrelated change".into();
    assert!(
        page.act(first.clone(), Action::Click, deadline())
            .await
            .is_ok()
    );
    fake.data.lock().unwrap().rows[0].record.entries[0].text = "changed record".into();
    assert!(
        page.act(first.clone(), Action::Click, deadline())
            .await
            .is_err()
    );
    assert_eq!(fake.effects.load(Ordering::Acquire), 1);
    let next = page
        .observe(Query::default(), 1, 64_000, read.cursor, deadline())
        .await
        .unwrap();
    assert!(page.act(first, Action::Click, deadline()).await.is_err());
    let current = next.rows[0].reference.clone().unwrap();
    page.pause();
    assert!(page.act(current, Action::Click, deadline()).await.is_err());
    assert_eq!(fake.effects.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn incomplete_material_and_invalid_query_never_submit_input() {
    let fake = Fake::new();
    fake.data.lock().unwrap().rows[0].record.complete = false;
    let page = Page::new(fake.clone());
    let read = page
        .observe(Query::default(), 1, 64_000, None, deadline())
        .await
        .unwrap();
    assert!(read.rows[0].reference.is_none());
    let changed = Query {
        find: Some("different".into()),
        ..Query::default()
    };
    assert!(
        page.observe(changed, 1, 64_000, read.cursor, deadline())
            .await
            .is_err()
    );
    assert_eq!(fake.effects.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn policy_reuses_site_rules_and_rejects_missing_connection_evidence() {
    use policy::{Capabilities, Policy};
    let sites: crate::browser::sites::Sites =
        serde_json::from_value(json!({"private_networks":"deny"})).unwrap();
    let rules = Policy::new(sites);
    assert!(
        rules
            .admit(Capabilities {
                navigation_intercept: true,
                resolved_destination_check: true,
                observed_connection_address: false
            })
            .is_err()
    );
    for url in [
        "http://127.0.0.1/",
        "http://2130706433/",
        "http://169.254.169.254/",
        "http://metadata.google.internal/",
        "file:///tmp/x",
        "http://user:password@example.com/",
    ] {
        assert!(!rules.allows(url, deadline()).await, "{url}");
    }
    rules.resolver(crate::browser::sites::resolver(|_| async {
        Some(vec!["169.254.169.254".parse().unwrap()])
    }));
    assert!(!rules.allows("https://rebinding.invalid/", deadline()).await);
    rules.resolver(crate::browser::sites::resolver(|_| async {
        std::future::pending().await
    }));
    assert!(
        !rules
            .allows(
                "https://slow.invalid/",
                Instant::now() + Duration::from_millis(10)
            )
            .await
    );
}

fn html() -> String {
    let mut html="<!doctype html><meta charset=utf-8><title>semantic_dom_v1</title><style>body{margin:20px;font:16px sans-serif}section{margin:12px;padding:8px;border:1px solid #ccc}input,button,textarea,select{font:inherit} [contenteditable]{min-height:24px}</style>".to_owned();
    for case in oracle().names {
        html += &format!(
            "<section data-rdh-record='{}'>{}</section>",
            case.id, case.html
        );
    }
    html += "<section data-rdh-record='multiline'><label for='multiline'>Multiline</label><textarea id='multiline'></textarea></section>";
    html += "<script>globalThis.persisted=false;globalThis.shownAt=0;addEventListener('pageshow',e=>{globalThis.persisted=e.persisted;globalThis.shownAt=Date.now();});globalThis.events=[];for(const name of ['keydown','keyup','beforeinput','input','change','click','submit'])document.addEventListener(name,e=>{events.push({type:e.type,id:e.target.id,key:e.key||'',trusted:e.isTrusted,data:e.data||'',ctrl:e.ctrlKey||false,meta:e.metaKey||false});if(name==='submit')e.preventDefault();},true);</script>";
    html
}
async fn server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let body = html();
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(move || {
            let body = body.clone();
            async move { axum::response::Html(body) }
        }),
    );
    let click_script = "<script>document.addEventListener('click',e=>top.postMessage({rdh_fixture:'click',id:e.target.id},'*'))</script>";
    let child = format!(
        "<!doctype html><meta charset=utf-8><button id='child'>Child</button><br><iframe style='width:420px;height:180px;border:3px solid;margin:25px' src='{url}grandchild'></iframe>{click_script}"
    );
    let app=app.route("/away",axum::routing::get(||async{axum::response::Html("<!doctype html><meta charset=utf-8><title>Away</title><button>Away</button>")}))
        .route("/child",axum::routing::get(move||{let html=child.clone();async move{axum::response::Html(html)}}))
        .route("/grandchild",axum::routing::get(move||async move{axum::response::Html(format!("<!doctype html><meta charset=utf-8><button id='deep' style='margin:20px;padding:15px'>Deep</button>{click_script}"))}))
        .route("/sibling",axum::routing::get(move||async move{axum::response::Html(format!("<!doctype html><meta charset=utf-8><button id='sibling'>Sibling</button>{click_script}"))}));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}
fn root(label: &str) -> PathBuf {
    let root = PathBuf::from(
        std::env::var_os("RDH_D3_EVIDENCE").expect("explicit private evidence directory"),
    );
    let directory = root.join(format!(
        "{label}-{}",
        crate::random::hex_token::<8>().unwrap()
    ));
    crate::storage::create_private_dir(&directory).unwrap();
    directory
}
fn record(name: &str, value: &Value) {
    let path = PathBuf::from(std::env::var_os("RDH_D3_EVIDENCE").unwrap()).join(name);
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .unwrap();
    writeln!(file, "{value}").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Chrome and evidence directory"]
async fn cdp_native_shared_oracle() {
    let (url, server) = server().await;
    let root = root("cdp");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut session = engine::semantic_cdp::Session::launch(
        &browser,
        &root,
        policy::Policy::new(Default::default()),
        deadline(),
    )
    .await
    .unwrap();
    session.navigate(&url, deadline()).await.unwrap();
    let output = names(session.port.clone()).await;
    record("cdp-names.json", &output);
    let output = input::assert_inputs(session.port.clone(), "cdp").await;
    record("cdp-input.json", &output);
    record(
        "cdp-observation.json",
        &observation::assert_observation(session.port.clone()).await,
    );
    record(
        "cdp-controls.json",
        &controls::assert_controls(session.port.clone()).await,
    );
    record(
        "cdp-frames.json",
        &frames::assert_frames(session.port.clone(), &url).await,
    );
    record(
        "cdp-ownership.json",
        &faults::assert_owned_inputs(session.port.clone(), "cdp").await,
    );
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Firefox helper and evidence directory"]
async fn bidi_native_shared_oracle() {
    let (url, server) = server().await;
    let root = root("bidi");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut session = engine::bidi::page::Session::open(
        &helper,
        &browser,
        &root,
        policy::Policy::new(Default::default()),
    )
    .await
    .unwrap();
    session.navigate(&url, deadline()).await.unwrap();
    let output = names(session.port.clone()).await;
    record("bidi-names.json", &output);
    let output = input::assert_inputs(session.port.clone(), "bidi").await;
    record("bidi-input.json", &output);
    record(
        "bidi-observation.json",
        &observation::assert_observation(session.port.clone()).await,
    );
    record(
        "bidi-controls.json",
        &controls::assert_controls(session.port.clone()).await,
    );
    record(
        "bidi-frames.json",
        &frames::assert_frames(session.port.clone(), &url).await,
    );
    record(
        "bidi-ownership.json",
        &faults::assert_owned_inputs(session.port.clone(), "bidi").await,
    );
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
