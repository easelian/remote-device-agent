//! Bounded, document-bound semantic observations and typed element actions.
pub use super::engine::Frame;
use super::{
    Failure, Result,
    engine::{Context, Node, PageActionPort, PageReadPort, Point, ReaderOptions, Scan},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub struct Element {
    frame: Frame,
    node: Node,
    signature: String,
    role: String,
    name: String,
    choices: Vec<String>,
    /// The element only looks like a control. The page vouches for it and names
    /// it, and it takes clicks and keys only.
    inferred: bool,
}

/// Why an element has a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A control that HTML defines.
    Native,
    /// An element whose role attribute names a control.
    Aria,
    /// An element that only looks like a control: its markup holds a click
    /// handler, the pointer turns into a hand over it, or it is in the focus
    /// order.
    Inferred,
}

impl Kind {
    /// The kind that the page reader gave a candidate, if it gave one.
    fn of(candidate: &Value) -> Option<Self> {
        match candidate["kind"].as_str()? {
            "native" => Some(Self::Native),
            "aria" => Some(Self::Aria),
            "inferred" => Some(Self::Inferred),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Aria => "aria",
            Self::Inferred => "inferred",
        }
    }
}

fn clipped(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
pub fn url_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none()
    })
}

pub async fn frame_tree(
    engine: &impl PageReadPort,
    session: &Context,
    deadline: Instant,
) -> Result<Vec<Frame>> {
    engine.frames(session, deadline).await
}

/// Confirms the frame still holds the observed document. Returns whether it is
/// the root frame of its target.
async fn current(engine: &impl PageReadPort, frame: &Frame, deadline: Instant) -> Result<bool> {
    let frames = frame_tree(engine, &frame.session, deadline).await?;
    if !frames
        .iter()
        .any(|f| f.id == frame.id && f.document == frame.document)
    {
        return Err(Failure::new("browser_stale_reference"));
    }
    Ok(frames.first().is_some_and(|f| f.id == frame.id))
}

pub async fn metadata(
    engine: &impl PageReadPort,
    frame: &Frame,
    deadline: Instant,
) -> Result<Value> {
    let value = engine.metadata(frame, deadline).await?;
    current(engine, frame, deadline).await?;
    Ok(value)
}

/// Whether the element that `data` describes holds a value that is picked: a
/// date, a time, a colour or a slider. Typed text does nothing to such a field.
fn picked(data: &Value) -> bool {
    data["tag"] == "INPUT"
        && matches!(
            data["type"].as_str(),
            Some("date" | "time" | "datetime-local" | "month" | "week" | "color" | "range")
        )
}

/// The roles by which the browser itself calls an element a control. It knows
/// of click handlers that the markup does not show, so what it calls a control
/// is one even where the page does not mark it.
fn interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "spinbutton"
            | "slider"
            | "switch"
            | "menuitem"
            | "tab"
            | "option"
    )
}

pub async fn scroll(
    engine: &impl PageActionPort,
    frame: &Frame,
    delta: &Value,
    deadline: Instant,
) -> Result<Value> {
    current(engine, frame, deadline).await?;
    let point = engine
        .scroll(
            frame,
            delta
                .as_f64()
                .ok_or_else(|| Failure::new("browser_invalid_params"))?,
            deadline,
        )
        .await?;
    Ok(
        json!({"scroll_x":point.x,"scroll_y":point.y,"input_acknowledged":true,"business_result_verified":false}),
    )
}

/// Which part of a page a read is asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Text and controls, in the order of the document.
    #[default]
    All,
    /// Controls and headings, without the text between them.
    Interactive,
    /// What lies in the part of the page that shows now.
    Viewport,
}

/// What a read is asked for. A sequence keeps it from its first call to its
/// last.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub filter: Filter,
    /// The one frame to read, by its id. Without it every frame is read.
    pub within: Option<String>,
    /// The text to look for. What is found is returned with its record.
    pub find: Option<String>,
}

/// How many places one search returns.
const PLACES: usize = 20;

impl Scope {
    /// What the page reader of one frame is told. `limit` is the number of
    /// places it may still find.
    fn options(&self, limit: usize) -> ReaderOptions {
        ReaderOptions {
            interactive: self.filter == Filter::Interactive,
            viewport: self.filter == Filter::Viewport,
            find: self.find.clone(),
            limit,
        }
    }
    /// How many nodes of a page one call looks at. A read that leaves most of
    /// the page out passes over it without asking the browser anything, so it
    /// looks further.
    fn reach(&self) -> usize {
        if self.filter == Filter::All && self.find.is_none() {
            5000
        } else {
            50_000
        }
    }
}

pub struct Cursor {
    scope: Scope,
    /// The tab's own document, whether or not it is among the frames to read.
    top: Frame,
    frames: Vec<Frame>,
    readers: Vec<Option<Scan>>,
    /// The places that the reader of each frame has found.
    places: Vec<usize>,
    frame: usize,
    pages: usize,
    scanned: usize,
    omitted: Vec<String>,
    /// The page changed something that the sequence had already read.
    changed: bool,
}

fn same_document(a: &Frame, b: &Frame) -> bool {
    a.session == b.session && a.id == b.id && a.document == b.document && a.url == b.url
}

/// A failure that means the thing being read is no longer there, as opposed to
/// the browser being lost or a deadline passing.
fn gone(error: &Failure) -> bool {
    matches!(
        error.code,
        "browser_stale_reference" | "browser_command_failed"
    )
}

impl Cursor {
    pub fn new(frames: &[Frame], scope: Scope) -> Result<Self> {
        if frames.is_empty() || frames.len() > 32 {
            return Err(Failure::new("browser_frame_limit"));
        }
        let top = frames[0].clone();
        let frames: Vec<Frame> = match &scope.within {
            Some(id) => frames.iter().filter(|f| f.id.v1() == id).cloned().collect(),
            None => frames.to_vec(),
        };
        // The frame that was asked for is not in the page, or is there no longer.
        if frames.is_empty() {
            return Err(Failure::new("browser_stale_reference"));
        }
        Ok(Self {
            scope,
            top,
            readers: vec![None; frames.len()],
            places: vec![0; frames.len()],
            frames,
            frame: 0,
            pages: 0,
            scanned: 0,
            omitted: Vec::new(),
            changed: false,
        })
    }
    /// A sequence can continue while the tab shows the document it began on.
    pub fn matches(&self, frames: &[Frame]) -> bool {
        frames
            .first()
            .is_some_and(|now| same_document(&self.top, now))
    }
    /// Notes subframes that came, went or navigated since the sequence began.
    /// It reads the ones it began with for as long as they last, and no others.
    /// A sequence that reads one frame looks at that frame only.
    pub fn compare(&mut self, frames: &[Frame]) {
        let same = match &self.scope.within {
            Some(_) => self
                .frames
                .iter()
                .all(|mine| frames.iter().any(|now| same_document(mine, now))),
            None => {
                self.frames.len() == frames.len()
                    && self
                        .frames
                        .iter()
                        .zip(frames)
                        .all(|(a, b)| same_document(a, b))
            }
        };
        if !same {
            self.omit("frame_changed");
        }
    }
    pub fn more(&self) -> bool {
        self.frame < self.frames.len()
    }
    fn omit(&mut self, reason: &str) {
        if !self.omitted.iter().any(|known| known == reason) {
            self.omitted.push(reason.into());
        }
    }
    /// Asks each reader whether it is still the current one and whether the page
    /// changed what it had read. A reader that a later observation replaced ends
    /// the sequence, and so does losing the reader of the tab's own document. A
    /// subframe's reader goes with its document, and takes only that frame.
    async fn check(&mut self, engine: &impl PageReadPort, deadline: Instant) -> Result<()> {
        for index in 0..self.frames.len() {
            let Some(reader) = &self.readers[index] else {
                continue;
            };
            let status = engine.reader_status(reader, deadline).await;
            match status {
                Ok(value) if !value.closed => {
                    self.changed |= value.changed;
                }
                Ok(_) => return Err(Failure::new("browser_stale_reference")),
                Err(error) if index > 0 && gone(&error) => {
                    self.readers[index] = None;
                    self.omit("frame_changed");
                    if index == self.frame {
                        self.frame += 1;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    pub async fn close(&self, engine: &impl PageReadPort, deadline: Instant) {
        for reader in self.readers.iter().flatten() {
            engine.close_reader(reader, deadline).await;
        }
    }
}

pub async fn observe(
    engine: &impl PageReadPort,
    frames: &[Frame],
    max_nodes: usize,
    max_chars: usize,
    deadline: Instant,
) -> Result<(Value, HashMap<String, Element>)> {
    let mut cursor = Cursor::new(frames, Scope::default())?;
    let result = observe_next(engine, &mut cursor, max_nodes, max_chars, deadline).await;
    cursor.close(engine, deadline).await;
    result
}

use super::engine::Found;

/// How many elements are described together.
const TOGETHER: usize = 16;

/// Adds the details that the page gave for the elements at `indexes`. An
/// element that left the page after the reader found it is reported as a stale
/// reference.
fn detailed(
    described: &mut HashMap<u64, Result<Option<Found>>>,
    indexes: &[u64],
    details: Result<Value>,
) {
    for (position, index) in indexes.iter().enumerate() {
        let data = match &details {
            Ok(reply) => &reply[position],
            Err(error) => {
                let error = Failure {
                    code: error.code,
                    state: error.state,
                };
                described.insert(*index, Err(error));
                continue;
            }
        };
        match described.get_mut(index) {
            Some(Ok(Some(found))) if data["connected"] == true => found.data = Some(data.clone()),
            Some(Ok(_)) => {
                described.insert(*index, Err(Failure::new("browser_stale_reference")));
            }
            // What went wrong while asking the browser stands.
            _ => {}
        }
    }
}

/// Describes several elements of the reader's current batch, by their indexes.
/// `controls` are the ones among them that the page marks as controls. The
/// browser is asked about all of the elements at once, and at the same time
/// the page is asked for the details of the controls in one call: on a slow or
/// busy device every call that is waited for alone costs a turn of waiting.
///
/// Each element has its own outcome. `None` means the browser no longer knows
/// it. An element that left the page after the reader found it is reported as
/// a stale reference.
async fn found(
    engine: &impl PageReadPort,
    frame: &Frame,
    reader: &Scan,
    indexes: &[u64],
    controls: &[u64],
    deadline: Instant,
) -> HashMap<u64, Result<Option<Found>>> {
    let (located, details) = tokio::join!(
        futures_util::future::join_all(
            indexes
                .iter()
                .map(|index| engine.locate(frame, reader, *index, deadline)),
        ),
        engine.details_of(reader, controls, deadline),
    );
    let mut described: HashMap<u64, Result<Option<Found>>> =
        indexes.iter().copied().zip(located).collect();
    detailed(&mut described, controls, details);
    // What the browser alone calls a control has a reference as well.
    let more: Vec<u64> = indexes
        .iter()
        .copied()
        .filter(|index| {
            !controls.contains(index)
                && matches!(&described[index], Ok(Some(found)) if found.exposed && interactive(&found.role))
        })
        .collect();
    let details = engine.details_of(reader, &more, deadline).await;
    detailed(&mut described, &more, details);
    described
}

/// Reads the next segment. The page may change while it is read: the read goes
/// on, and the coverage says so. It fails only when the tab's own document is
/// replaced or a later observation takes the reader's place.
pub async fn observe_next(
    engine: &impl PageReadPort,
    cursor: &mut Cursor,
    max_nodes: usize,
    max_chars: usize,
    deadline: Instant,
) -> Result<(Value, HashMap<String, Element>)> {
    let work_until = deadline.min(Instant::now() + Duration::from_secs(2));
    cursor.check(engine, deadline).await?;
    let (filter, reach) = (cursor.scope.filter, cursor.scope.reach());
    let observation = crate::random::hex_token::<12>().map_err(super::io_error)?;
    let mut nodes = Vec::new();
    let mut refs = HashMap::new();
    let mut remaining = max_chars;
    let mut visited = 0usize;
    let mut omitted = Vec::<String>::new();
    let mut observed = std::collections::HashSet::new();
    let mut changed = false;
    while cursor.more() {
        if nodes.len() >= max_nodes
            || remaining == 0
            || visited >= reach
            || Instant::now() + Duration::from_millis(200) >= work_until
        {
            break;
        }
        let index = cursor.frame;
        let frame = cursor.frames[index].clone();
        let result: Result<bool> = async {
            current(engine, &frame, deadline).await?;
            if cursor.readers[index].is_none() {
                cursor.readers[index] = Some(
                    engine
                        .open_reader(
                            &frame,
                            cursor.scope.options(
                                PLACES.saturating_sub(cursor.places.iter().sum::<usize>()),
                            ),
                            deadline,
                        )
                        .await?,
                );
            }
            let object = cursor.readers[index].clone().unwrap();
            let summary = engine
                .collect(
                    &object,
                    (max_nodes - nodes.len()).min(200),
                    (reach - visited).min(5000),
                    deadline,
                )
                .await?;
            // A reader that a later observation closed returns no nodes.
            let candidates = &summary.nodes;
            changed |= summary.changed;
            visited += summary.scanned;
            cursor.places[index] = summary.matches;
            omitted.extend(summary.omitted);
            let mut processed = 0;
            let mut partial_chars = 0;
            // Elements are described a few at a time, this one and the ones after it.
            let mut described: HashMap<u64, Result<Option<Found>>> = HashMap::new();
            for (position, candidate) in candidates.iter().enumerate() {
                if remaining == 0
                    || nodes.len() >= max_nodes
                    || Instant::now() + Duration::from_millis(200) >= work_until
                {
                    break;
                }
                processed += 1;
                // The kind of control that the page makes of the element, if any.
                let marked = Kind::of(candidate);
                let element = match candidate["index"].as_u64() {
                    Some(index) => {
                        if !described.contains_key(&index) {
                            let ahead: Vec<&Value> = candidates[position..]
                                .iter()
                                .filter(|c| c["index"].is_u64())
                                .take(TOGETHER)
                                .collect();
                            let indexes: Vec<u64> =
                                ahead.iter().filter_map(|c| c["index"].as_u64()).collect();
                            let controls: Vec<u64> = ahead
                                .iter()
                                .filter(|c| Kind::of(c).is_some())
                                .filter_map(|c| c["index"].as_u64())
                                .collect();
                            described =
                                found(engine, &frame, &object, &indexes, &controls, deadline).await;
                        }
                        match described
                            .remove(&index)
                            .expect("described with the elements after it")
                        {
                            // The accessibility tree leaves out what is hidden. It also leaves out
                            // much of what only looks like a control, and there the page has
                            // already seen that the element shows.
                            Ok(Some(element))
                                if element.exposed || marked == Some(Kind::Inferred) =>
                            {
                                Some(element)
                            }
                            Ok(_) => continue,
                            // The element left the page after the reader found it. That is a
                            // change to report, unless its whole document went with it.
                            Err(error) if gone(&error) => {
                                current(engine, &frame, deadline).await?;
                                changed = true;
                                continue;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    None => None,
                };
                // An element with details is a control: by what the page says of it, or
                // else by what the browser calls it.
                let kind = element
                    .as_ref()
                    .and_then(|found| found.data.as_ref())
                    .map(|_| marked.unwrap_or(Kind::Native));
                let (role, full_name) = match &element {
                    // The browser's names for such an element differ between versions, and
                    // are often empty. The page names it.
                    Some(found) if kind == Some(Kind::Inferred) => (
                        if found.exposed && !found.role.is_empty() {
                            found.role.as_str()
                        } else {
                            "generic"
                        },
                        found
                            .data
                            .as_ref()
                            .and_then(|data| data["words"].as_str())
                            .unwrap_or(""),
                    ),
                    Some(found) => (found.role.as_str(), found.name.as_str()),
                    None => (
                        candidate["role"].as_str().unwrap_or(""),
                        candidate["name"].as_str().unwrap_or(""),
                    ),
                };
                let name = clipped(full_name, 512.min(remaining));
                if element.is_some() && name.len() < full_name.len() {
                    omitted.push("text_clipped".into());
                }
                if name.is_empty() && kind.is_none() {
                    continue;
                }
                // The page reader leaves the text out. What the browser alone names, such
                // as a region, is left out here.
                if filter == Filter::Interactive && kind.is_none() && role != "heading" {
                    continue;
                }
                remaining = remaining.saturating_sub(name.chars().count() + role.len());
                let mut visible = json!({"role":role,"name":name,"frame_id":frame.id.v1()});
                if element.is_none() {
                    visible["text_continued"] = candidate["text_continued"].clone();
                    visible["text_remaining"] = (candidate["text_remaining"] == true
                        || name.len() < full_name.len())
                    .into();
                }
                if let (
                    Some(Found {
                        node,
                        value,
                        data: Some(data),
                        ..
                    }),
                    Some(kind),
                ) = (&element, kind)
                {
                    visible["kind"] = kind.name().into();
                    let mut choices = Vec::new();
                    if let Some(options) = data["select"]["options"].as_array() {
                        let mut shown = Vec::new();
                        for option in options {
                            let size = ["value", "label"]
                                .iter()
                                .filter_map(|k| option[*k].as_str())
                                .map(|v| v.chars().count())
                                .sum::<usize>()
                                + 32;
                            if size > remaining {
                                break;
                            }
                            remaining -= size;
                            if option["disabled"] != true
                                && let Some(value) = option["value"].as_str()
                            {
                                choices.push(value.into());
                            }
                            shown.push(option.clone());
                        }
                        let complete =
                            data["select"]["complete"] == true && shown.len() == options.len();
                        visible["options"] = shown.into();
                        visible["options_complete"] = complete.into();
                        visible["multiple"] = data["select"]["multiple"].clone();
                        if !complete {
                            omitted.push("select_options_omitted".into());
                        }
                    }
                    visible["enabled"] = data["enabled"].clone();
                    visible["editable"] = data["editable"].clone();
                    visible["sensitive"] = (data["type"] == "password").into();
                    visible["file_input"] = (data["type"] == "file").into();
                    // A number field or a slider holds a number, not a string.
                    let value = match value {
                        Value::String(value) => Some(value.clone()),
                        Value::Number(value) => Some(
                            value
                                .as_f64()
                                .filter(|v| v.fract() == 0. && v.abs() < 1e15)
                                .map_or_else(|| value.to_string(), |v| format!("{v:.0}")),
                        ),
                        _ => None,
                    };
                    if data["type"] != "password"
                        && data["type"] != "file"
                        && let Some(value) = value.as_deref()
                    {
                        let shown = clipped(value, remaining.min(512));
                        remaining = remaining.saturating_sub(shown.chars().count());
                        if shown.len() < value.len() {
                            omitted.push("value_clipped".into());
                        }
                        visible["value"] = shown.into();
                    }
                    if matches!(data["type"].as_str(), Some("checkbox" | "radio")) {
                        visible["checked"] = data["checked"].clone();
                    }
                    if data["steady"] == false {
                        // A reference is bound to the text of the control's record. That text
                        // changed after it was reported, so a reference would be bound to
                        // something other than what the caller was shown.
                        visible["changed_during_read"] = true.into();
                        changed = true;
                    } else {
                        let reference = format!("e_{observation}_{}", refs.len());
                        visible["ref"] = reference.clone().into();
                        refs.insert(
                            reference,
                            Element {
                                frame: frame.clone(),
                                node: node.clone(),
                                signature: crate::config::sha256_hex(
                                    data["signature"].as_str().unwrap_or("").as_bytes(),
                                ),
                                role: role.into(),
                                name: crate::config::sha256_hex(full_name.as_bytes()),
                                choices,
                                inferred: kind == Kind::Inferred,
                            },
                        );
                    }
                }
                nodes.push(visible);
                if element.is_none() && name.len() < full_name.len() {
                    processed -= 1;
                    partial_chars = name.chars().count();
                    break;
                }
            }
            let ack = engine
                .ack(&object, processed, partial_chars, deadline)
                .await?;
            if ack.closed {
                return Err(Failure::new("browser_stale_reference"));
            }
            changed |= ack.changed;
            current(engine, &frame, deadline).await?;
            Ok(ack.more)
        }
        .await;
        engine.release_scratch(&frame.session, deadline).await;
        observed.insert(frame.id.clone());
        match result {
            Ok(true) => {}
            Ok(false) => cursor.frame += 1,
            // A subframe that navigated or went away takes only its own content
            // with it. The tab's own document being replaced ends the read.
            Err(error) if index > 0 && gone(&error) => {
                current(engine, &cursor.frames[0], deadline).await?;
                cursor.omit("frame_changed");
                cursor.frame += 1;
            }
            Err(error) => return Err(error),
        }
    }
    cursor.check(engine, deadline).await?;
    cursor.changed |= changed;
    cursor.scanned += visited;
    cursor.pages += 1;
    cursor.omitted.extend(omitted);
    cursor.omitted.sort();
    cursor.omitted.dedup();
    let mut omitted = cursor.omitted.clone();
    if cursor.more() {
        omitted.push("observation_budget".into());
    }
    let complete = !cursor.more() && cursor.omitted.is_empty() && !cursor.changed;
    let mut coverage = json!({
        "scope":"document_text_and_controls_open_shadow","complete_within_scope":complete,
        "omitted":omitted,"changed_during_read":cursor.changed,
        "frames_observed":observed.len(),"frames_omitted":cursor.frames.len()-cursor.frame,
        "frames_completed":cursor.frame,"nodes_scanned":visited,"total_nodes_scanned":cursor.scanned,
        "page_index":cursor.pages,"more":cursor.more(),"sequence_complete":complete,
    });
    // A read that was asked for part of the page says which part. It is complete
    // when it holds all of that part.
    match filter {
        Filter::All => {}
        Filter::Interactive => coverage["filter"] = "interactive".into(),
        Filter::Viewport => coverage["filter"] = "viewport".into(),
    }
    if let Some(frame) = &cursor.scope.within {
        coverage["within"] = frame.as_str().into();
    }
    if cursor.scope.find.is_some() {
        coverage["matches"] = cursor.places.iter().sum::<usize>().into();
    }
    Ok((
        json!({"observation_id":observation,"nodes":nodes,"coverage":coverage}),
        refs,
    ))
}

impl Element {
    pub async fn prepare_file(
        &self,
        engine: &impl PageActionPort,
        top: &Frame,
        deadline: Instant,
    ) -> Result<String> {
        // The consent UI names the top-level site. Cross-origin/nested frames
        // require a separate presentation contract before they can receive files.
        if self.frame.id != top.id || self.frame.session != top.session {
            return Err(Failure::new("browser_upload_denied"));
        }
        let data = self.checked(engine, deadline).await?;
        if data["tag"] != "INPUT" || data["type"] != "file" {
            return Err(Failure::new("browser_invalid_params"));
        }
        let page = metadata(engine, &self.frame, deadline).await?;
        origin(page["url"].as_str().unwrap_or(""))
    }
    pub async fn attach_file(
        &self,
        engine: &impl PageActionPort,
        path: &std::path::Path,
        deadline: Instant,
    ) -> Result<Value> {
        engine.attach_file(&self.node, path, deadline).await?;
        Ok(json!({"file_selected":true,"business_result_verified":false}))
    }
    async fn ancestor_hit(
        &self,
        engine: &impl PageActionPort,
        mut x: f64,
        mut y: f64,
        deadline: Instant,
    ) -> Result<()> {
        let mut child_session = &self.frame.session;
        for (parent_session, child_frame) in &self.frame.ancestors {
            let viewport = engine.layout(child_session, deadline).await?;
            let (width, height) = (viewport.width, viewport.height);
            if width <= 0. || height <= 0. || x < 0. || y < 0. || x >= width || y >= height {
                return Err(Failure::new("browser_not_actionable"));
            }
            let owner = engine
                .frame_owner(parent_session, child_frame, deadline)
                .await?;
            let q = engine.quad(&owner, deadline).await?;
            let (u, v) = (x / width, y / height);
            x = q[0] * (1. - u) * (1. - v)
                + q[2] * u * (1. - v)
                + q[4] * u * v
                + q[6] * (1. - u) * v;
            y = q[1] * (1. - u) * (1. - v)
                + q[3] * u * (1. - v)
                + q[5] * u * v
                + q[7] * (1. - u) * v;
            let hit = engine
                .hit_at(parent_session, Point { x, y }, deadline)
                .await?;
            if hit.frame != *child_frame && hit.node != owner {
                return Err(Failure::new("browser_not_actionable"));
            }
            child_session = parent_session;
        }
        Ok(())
    }
    async fn point(&self, engine: &impl PageActionPort, deadline: Instant) -> Result<Point> {
        // Browser input uses the target's main-frame viewport. A DOM rect from a
        // same-process subframe is in that frame's coordinates, so never send it.
        let points = engine.quad(&self.node, deadline).await?;
        let x = (points[0] + points[2] + points[4] + points[6]) / 4.;
        let y = (points[1] + points[3] + points[5] + points[7]) / 4.;
        if !x.is_finite() || !y.is_finite() || x < 0. || y < 0. {
            return Err(Failure::new("browser_not_actionable"));
        }
        let hit = engine
            .hit_at(&self.frame.session, Point { x, y }, deadline)
            .await?;
        if hit.frame != self.frame.id {
            return Err(Failure::new("browser_not_actionable"));
        }
        self.ancestor_hit(engine, x, y, deadline).await?;
        Ok(Point { x, y })
    }
    async fn checked(&self, engine: &impl PageActionPort, deadline: Instant) -> Result<Value> {
        let root = current(engine, &self.frame, deadline).await?;
        let mut data = engine.details(&self.frame, &self.node, deadline).await?;
        if data["connected"] != true
            || crate::config::sha256_hex(data["signature"].as_str().unwrap_or("").as_bytes())
                != self.signature
        {
            return Err(Failure::new("browser_stale_reference"));
        }
        if self.inferred {
            // The page named this element, so the page says whether the name stands.
            if crate::config::sha256_hex(data["words"].as_str().unwrap_or("").as_bytes())
                != self.name
            {
                return Err(Failure::new("browser_stale_reference"));
            }
        } else {
            let ax = engine.accessibility(&self.node, deadline).await?;
            if !ax.iter().any(|n| {
                n.role == self.role && crate::config::sha256_hex(n.name.as_bytes()) == self.name
            }) {
                return Err(Failure::new("browser_stale_reference"));
            }
        }
        if data["enabled"] != true || data["visible"] != true {
            return Err(Failure::new("browser_not_actionable"));
        }
        // The tab's top document: the root frame of a target that has no parent.
        data["top"] = (root && self.frame.ancestors.is_empty()).into();
        data["inferred"] = self.inferred.into();
        Ok(data)
    }

    /// Sets the value of a field whose value is picked. Without `apply` it only
    /// asks whether the field can hold `value`, which changes nothing.
    async fn set(
        &self,
        engine: &impl PageActionPort,
        value: &Value,
        apply: bool,
        deadline: Instant,
    ) -> Result<Option<bool>> {
        engine
            .set_form_value(
                &self.frame,
                &self.node,
                value
                    .as_str()
                    .ok_or_else(|| Failure::new("browser_invalid_params"))?,
                apply,
                deadline,
            )
            .await
    }

    pub async fn act(
        &self,
        engine: &impl PageActionPort,
        action: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        let data = self.checked(engine, deadline).await?;
        // Nothing has been sent yet, so an action that does not suit this
        // element is a plain rejection instead of an uncertain outcome.
        if precheck(action, &data, &self.choices)? {
            return Ok(json!({"changed":false}));
        }
        // An element that only looks like a control can hold real ones, as a card
        // holds its delete button. A click meant for the card must not be taken
        // by one of them.
        let taken = |data: &Value| {
            action["kind"] == "click" && data["inferred"] == true && data["inner"] == true
        };
        // Scrolling into view is an effect command, and it does nothing for an
        // element that is already fully in view. Skip it there, so that a
        // target which something covers is refused before any effect. Only the
        // top document can tell: a subframe sees just its own viewport. An
        // element that is neither hit nor painted at its click point is clipped
        // by something the page function cannot see, and still scrolls.
        if data["top"] == true
            && data["in_view"] == true
            && (data["hit"] == true || data["covered"] == true)
        {
            if data["covered"] == true || taken(&data) {
                return Err(Failure::new("browser_not_actionable"));
            }
        } else {
            engine.scroll_into_view(&self.node, deadline).await?;
        }
        let first = self.point(engine, deadline).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let data = self.checked(engine, deadline).await?;
        let point = self.point(engine, deadline).await?;
        if data["hit"] != true || first.x != point.x || first.y != point.y || taken(&data) {
            return Err(Failure::new("browser_not_actionable"));
        }
        // The element can change while the page settles.
        if precheck(action, &data, &self.choices)? {
            return Ok(json!({"changed":false}));
        }
        match action["kind"].as_str().unwrap_or("") {
            "select" => {
                let value = action["value"].as_str().unwrap_or("");
                if !engine
                    .select(&self.frame, &self.node, value, deadline)
                    .await?
                {
                    return Err(Failure::unknown("browser_not_actionable"));
                }
            }
            "click" | "check" => {
                engine.click(&self.frame.session, point, deadline).await?;
            }
            "fill" if picked(&data) => {
                // Asked before anything is sent, so that a value the field cannot
                // hold is a plain rejection.
                if self.set(engine, &action["text"], false, deadline).await? != Some(true) {
                    return Err(Failure::new("browser_invalid_params"));
                }
                if self.set(engine, &action["text"], true, deadline).await? != Some(true) {
                    return Err(Failure::unknown("browser_not_actionable"));
                }
            }
            "fill" => {
                engine.focus(&self.node, deadline).await?;
                engine.select_all(&self.frame.session, deadline).await?;
                engine
                    .insert_text(
                        &self.frame.session,
                        action["text"].as_str().unwrap_or(""),
                        deadline,
                    )
                    .await?;
            }
            "press" => {
                let key = action["key"].as_str().unwrap_or("");
                supported_key(key).ok_or_else(|| Failure::new("browser_invalid_params"))?;
                engine.focus(&self.node, deadline).await?;
                engine.press(&self.frame.session, key, deadline).await?;
            }
            _ => return Err(Failure::new("browser_invalid_params")),
        }
        Ok(json!({"input_acknowledged":true,"business_result_verified":false}))
    }
}

/// Decides, without any page effect, whether `action` suits the element that
/// `data` describes. `Ok(true)` means the element already holds the requested
/// state and needs no input.
fn precheck(action: &Value, data: &Value, choices: &[String]) -> Result<bool> {
    let invalid = || Failure::new("browser_invalid_params");
    let kind = action["kind"].as_str().unwrap_or("");
    // An element that only looks like a control takes clicks and keys, and
    // nothing that needs a real field, box or list.
    if data["inferred"] == true && !matches!(kind, "click" | "press") {
        return Err(invalid());
    }
    match kind {
        "click" => {}
        "press" => {
            supported_key(action["key"].as_str().unwrap_or("")).ok_or_else(invalid)?;
        }
        "check" => {
            if !matches!(data["type"].as_str(), Some("checkbox" | "radio")) {
                return Err(invalid());
            }
            return Ok(data["checked"] == action["checked"]);
        }
        "select" => {
            let value = action["value"].as_str().unwrap_or("");
            if data["tag"] != "SELECT"
                || data["select"]["multiple"] == true
                || !choices.iter().any(|v| v == value)
            {
                return Err(invalid());
            }
        }
        "fill" => {
            if data["type"] == "password" {
                return Err(Failure::new("browser_sensitive_input"));
            }
            // A read-only field still takes clicks and keys, only not text.
            if data["readonly"] == true {
                return Err(Failure::new("browser_not_actionable"));
            }
            if data["editable"] != true {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(false)
}

fn supported_key(key: &str) -> Option<()> {
    matches!(
        key,
        "Enter"
            | "Tab"
            | "Escape"
            | "Backspace"
            | "ArrowDown"
            | "ArrowUp"
            | "ArrowLeft"
            | "ArrowRight"
    )
    .then_some(())
}

pub fn origin(value: &str) -> Result<String> {
    if !url_allowed(value) {
        return Err(Failure::new("browser_url_denied"));
    }
    Ok(url::Url::parse(value)
        .map_err(super::io_error)?
        .origin()
        .ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejection(
        action: Value,
        data: &Value,
        choices: &[&str],
    ) -> Option<(&'static str, &'static str)> {
        let choices: Vec<String> = choices.iter().map(|v| (*v).to_owned()).collect();
        precheck(&action, data, &choices)
            .err()
            .map(|error| (error.code, error.state))
    }

    #[test]
    fn unsuitable_actions_are_rejected_without_a_page_effect() {
        let text = json!({"tag":"INPUT","type":"text","editable":true});
        let password = json!({"tag":"INPUT","type":"password","editable":true});
        let button = json!({"tag":"BUTTON","type":"submit","editable":false});
        let single = json!({"tag":"SELECT","type":"select-one","editable":false,"select":{"multiple":false}});
        let multiple = json!({"tag":"SELECT","type":"select-multiple","editable":false,"select":{"multiple":true}});
        let invalid = Some(("browser_invalid_params", "not_sent"));

        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &text, &[]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &password, &[]),
            Some(("browser_sensitive_input", "not_sent"))
        );
        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &button, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &single, &["red"]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"blue"}), &single, &["red"]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &multiple, &["red"]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &text, &["red"]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"check","checked":true}), &text, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"press","key":"Enter"}), &button, &[]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"press","key":"F5"}), &button, &[]),
            invalid
        );
        assert_eq!(rejection(json!({"kind":"click"}), &button, &[]), None);
        assert_eq!(
            rejection(json!({"kind":"attach_file"}), &button, &[]),
            invalid
        );
    }

    #[test]
    fn an_element_that_only_looks_like_a_control_takes_clicks_and_keys_only() {
        // The page would let this one be edited and checked. It is still not a field.
        let card = json!({"tag":"DIV","type":"checkbox","editable":true,"inferred":true,
            "select":{"multiple":false}});
        let invalid = Some(("browser_invalid_params", "not_sent"));
        assert_eq!(rejection(json!({"kind":"click"}), &card, &[]), None);
        assert_eq!(
            rejection(json!({"kind":"press","key":"Enter"}), &card, &[]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &card, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"check","checked":true}), &card, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &card, &["red"]),
            invalid
        );
    }

    fn frame(id: &str, document: &str, url: &str) -> Frame {
        crate::browser::engine::testing::Ids::stable().frame("session", id, document, url)
    }

    #[test]
    fn a_sequence_continues_while_the_tab_shows_the_document_it_began_on() {
        let begun = [
            frame("top", "1", "https://shop.test/orders"),
            frame("ad", "2", "https://ads.test/first"),
        ];
        let mut cursor = Cursor::new(&begun, Scope::default()).unwrap();
        assert!(cursor.matches(&begun));
        cursor.compare(&begun);
        assert!(cursor.omitted.is_empty());

        // A subframe that navigated, or one that went away, is named as left out.
        let rotated = [
            begun[0].clone(),
            frame("ad", "3", "https://ads.test/second"),
        ];
        assert!(cursor.matches(&rotated));
        assert!(cursor.matches(&begun[..1]));
        cursor.compare(&rotated);
        cursor.compare(&begun[..1]);
        assert_eq!(cursor.omitted, ["frame_changed"]);

        // The tab's own document being replaced, or its address changing, ends it.
        assert!(!cursor.matches(&[frame("top", "9", "https://shop.test/orders")]));
        assert!(!cursor.matches(&[frame("top", "1", "https://shop.test/orders/7")]));
        assert!(!cursor.matches(&[]));
    }

    #[test]
    fn a_sequence_that_reads_one_frame_looks_at_that_frame_only() {
        let begun = [
            frame("top", "1", "https://shop.test/orders"),
            frame("ad", "2", "https://ads.test/first"),
            frame("pay", "3", "https://pay.test/form"),
        ];
        let within = |id: &str| Scope {
            within: Some(id.into()),
            ..Scope::default()
        };
        let mut cursor = Cursor::new(&begun, within("pay")).unwrap();
        assert_eq!(cursor.frames.len(), 1);
        assert_eq!(cursor.frames[0].id.v1(), "pay");

        // Another frame that navigates or goes away is none of its business.
        let rotated = [
            begun[0].clone(),
            frame("ad", "9", "https://ads.test/second"),
            begun[2].clone(),
        ];
        assert!(cursor.matches(&rotated));
        cursor.compare(&rotated);
        cursor.compare(&[begun[0].clone(), begun[2].clone()]);
        assert!(cursor.omitted.is_empty());
        // Its own frame navigating is.
        cursor.compare(&[begun[0].clone(), frame("pay", "4", "https://pay.test/done")]);
        assert_eq!(cursor.omitted, ["frame_changed"]);
        // It still ends with the tab's own document.
        assert!(!cursor.matches(&[
            frame("top", "9", "https://shop.test/orders"),
            begun[2].clone()
        ]));

        // A frame that the page does not hold cannot be read.
        let missing = Cursor::new(&begun, within("gone")).err().unwrap();
        assert_eq!(
            (missing.code, missing.state),
            ("browser_stale_reference", "not_sent")
        );
    }

    #[test]
    fn a_checkbox_that_already_matches_needs_no_input() {
        let checked = json!({"tag":"INPUT","type":"checkbox","checked":true});
        assert!(precheck(&json!({"kind":"check","checked":true}), &checked, &[]).unwrap());
        assert!(!precheck(&json!({"kind":"check","checked":false}), &checked, &[]).unwrap());
    }

    #[test]
    fn a_read_only_field_takes_clicks_and_keys_but_not_text() {
        let field = json!({"tag":"INPUT","type":"text","editable":false,"readonly":true});
        let secret = json!({"tag":"INPUT","type":"password","editable":false,"readonly":true});
        let fill = || json!({"kind":"fill","text":"x"});
        assert_eq!(rejection(json!({"kind":"click"}), &field, &[]), None);
        assert_eq!(
            rejection(json!({"kind":"press","key":"Enter"}), &field, &[]),
            None
        );
        assert_eq!(
            rejection(fill(), &field, &[]),
            Some(("browser_not_actionable", "not_sent"))
        );
        assert_eq!(
            rejection(fill(), &secret, &[]),
            Some(("browser_sensitive_input", "not_sent"))
        );
    }
}

#[cfg(test)]
#[path = "engine/cdp/page_tests.rs"]
mod protocol_tests;

#[cfg(test)]
mod fake_tests;
