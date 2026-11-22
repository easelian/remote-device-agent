//! The interface an AI works with: three operations that reply in text.
//!
//! `browser_do` carries out a list of steps and says what each one led to,
//! `browser_read` reads the page, `browser_see` takes a picture of it. A call
//! names no control epoch and, with one session open, no session. Every step
//! is carried out by the session operations of the parent module, so the site
//! policy, the hand-over to a person and the file approvals apply unchanged.
use super::*;
use std::fmt::Write as _;

pub(in crate::browser::runtime) const OPERATIONS: [&str; 3] =
    ["browser_do", "browser_read", "browser_see"];
const MAX_STEPS: usize = 16;
const MAX_CANDIDATES: usize = 8;
/// How long a step waits for its target to appear or for the page to stop
/// replacing itself, as a person would before giving up.
const PATIENCE: Duration = Duration::from_secs(4);
const RETRY: Duration = Duration::from_millis(100);
/// Pages of a search that are read to find a target.
const SEARCH_PAGES: usize = 8;

/// Failures that say the page was busy changing, not that the step is wrong.
fn passing(stop: &Stop) -> bool {
    stop.state == "not_sent"
        && matches!(
            stop.code,
            "browser_stale_reference"
                | "browser_stale_frame"
                | "browser_command_failed"
                | "browser_target_not_found"
                | "browser_busy"
        )
}

fn invalid() -> Failure {
    Failure::new("browser_invalid_params")
}

/// What a step is aimed at: a reference from an earlier reply, or a
/// description that is looked up on the page as it is now.
#[derive(Debug, PartialEq)]
enum Target {
    Reference(String),
    Described {
        name: String,
        role: Option<String>,
        within: Option<String>,
        nth: Option<usize>,
    },
}
impl Target {
    fn parse(value: &Value) -> Result<Self> {
        if let Some(reference) = value.as_str() {
            return (!reference.is_empty() && reference.len() <= 128)
                .then(|| Self::Reference(reference.to_owned()))
                .ok_or_else(invalid);
        }
        let map = value.as_object().ok_or_else(invalid)?;
        if map
            .keys()
            .any(|key| !["name", "role", "in", "nth"].contains(&key.as_str()))
        {
            return Err(invalid());
        }
        let text = |key: &str| match map.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .filter(|text| !text.trim().is_empty() && text.chars().count() <= 128)
                .map(|text| Some(text.trim().to_owned()))
                .ok_or_else(invalid),
        };
        let nth = match map.get("nth") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|nth| (1..=1000).contains(nth))
                    .ok_or_else(invalid)? as usize,
            ),
        };
        Ok(Self::Described {
            name: text("name")?.ok_or_else(invalid)?,
            role: text("role")?,
            within: text("in")?,
            nth,
        })
    }
    fn written(&self) -> String {
        match self {
            Self::Reference(reference) => format!("[{reference}]"),
            Self::Described {
                name, role, within, ..
            } => {
                let mut text = format!("{} {name:?}", role.as_deref().unwrap_or("control"));
                if let Some(within) = within {
                    let _ = write!(text, " in {within:?}");
                }
                text
            }
        }
    }
}

/// Why a step did not finish, in the words the reply uses.
struct Stop {
    code: &'static str,
    state: &'static str,
    detail: String,
}
impl From<Failure> for Stop {
    fn from(failure: Failure) -> Self {
        Self {
            code: failure.code,
            state: failure.state,
            detail: String::new(),
        }
    }
}
fn advice(code: &str) -> &'static str {
    match code {
        "browser_stale_reference" => {
            "that reference no longer stands for an element; describe the target by name or read the page again"
        }
        "browser_not_actionable" => "the element is hidden, disabled or covered right now",
        "browser_dialog_open" => {
            "a dialog is open; answer it with {\"dialog\":\"accept\"} or {\"dialog\":\"dismiss\"}"
        }
        "browser_paused" => {
            "the browser is with a person or was paused; {\"resume\":true} takes it back once they are done"
        }
        "browser_dialog_unavailable" => "no dialog is open",
        "browser_input_pending" => {
            "an earlier input has not finished; a dialog may be waiting for an answer"
        }
        "browser_lost" | "browser_session_unavailable" => {
            "this session has ended; start again with an open step"
        }
        "browser_site_denied" | "browser_url_denied" => {
            "the device owner's site policy refuses this address"
        }
        "browser_condition_timeout" => "the condition did not come true in time",
        "browser_target_not_found" => {
            "nothing on the page matches; read the page to see what is there"
        }
        "browser_target_ambiguous" => "narrow it with \"in\", \"role\" or \"nth\"",
        "browser_session_ambiguous" => "more than one session is open; name one with \"session\"",
        "browser_no_session" => "no session is open; begin with {\"open\":\"https://…\"}",
        _ => "",
    }
}

/// One element of a structured observation, as far as matching needs it.
struct Candidate {
    reference: String,
    role: String,
    name: String,
    record: String,
}
impl Candidate {
    fn line(&self) -> String {
        let mut line = format!("  {} {:?}", self.role, self.name);
        let around = self.around();
        if !around.is_empty() {
            let _ = write!(line, " in {around:?}");
        }
        line
    }
}
impl Candidate {
    /// The text of the record around the element, without its own name.
    fn around(&self) -> String {
        let rest = self.record.replace(&self.name, " ");
        rest.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect()
    }
}
fn candidates(observation: &Value) -> Vec<Candidate> {
    observation["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let record = node["record"]["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|entry| [entry["text"].as_str(), entry["value"].as_str()])
                .flatten()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Some(Candidate {
                reference: node["ref"].as_str()?.to_owned(),
                role: node["role"]
                    .as_str()
                    .filter(|role| !role.is_empty())
                    .or(node["tag"].as_str())
                    .unwrap_or("")
                    .to_owned(),
                name: node["name"].as_str().unwrap_or("").trim().to_owned(),
                record,
            })
        })
        .collect()
}
/// The one element a description stands for. Several equally good matches are
/// listed for the caller to choose from; none is picked on its behalf.
fn choose(target: &Target, all: &[Candidate]) -> std::result::Result<usize, Stop> {
    let Target::Described {
        name,
        role,
        within,
        nth,
    } = target
    else {
        unreachable!()
    };
    let lower = name.to_lowercase();
    let fits = |candidate: &Candidate| {
        role.as_ref()
            .is_none_or(|role| candidate.role.eq_ignore_ascii_case(role))
            && within.as_ref().is_none_or(|within| {
                candidate
                    .record
                    .to_lowercase()
                    .contains(&within.to_lowercase())
            })
    };
    // An exact name beats one that differs in case, which beats one that only
    // contains the words asked for.
    let grades: [&dyn Fn(&Candidate) -> bool; 3] = [
        &|candidate| candidate.name == *name,
        &|candidate| candidate.name.to_lowercase() == lower,
        &|candidate| candidate.name.to_lowercase().contains(&lower),
    ];
    for grade in grades {
        let found: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, candidate)| fits(candidate) && grade(candidate))
            .map(|(index, _)| index)
            .collect();
        match (found.len(), nth) {
            (0, _) => continue,
            (1, None | Some(1)) => return Ok(found[0]),
            (count, Some(nth)) if *nth <= count => return Ok(found[nth - 1]),
            (count, _) => {
                let mut detail = format!("{count} match");
                for index in found.iter().take(MAX_CANDIDATES) {
                    let _ = write!(detail, "\n{}", all[*index].line());
                }
                if count > MAX_CANDIDATES {
                    let _ = write!(detail, "\n  … and {} more", count - MAX_CANDIDATES);
                }
                return Err(Stop {
                    code: "browser_target_ambiguous",
                    state: "not_sent",
                    detail,
                });
            }
        }
    }
    Err(Stop {
        code: "browser_target_not_found",
        state: "not_sent",
        detail: String::new(),
    })
}

/// The verb of a step and the action it becomes once its targets are known.
struct Step<'a> {
    verb: &'static str,
    map: &'a serde_json::Map<String, Value>,
}
const VERBS: [&str; 16] = [
    "open", "click", "hover", "fill", "select", "check", "press", "scroll", "drag", "upload",
    "wait", "dialog", "tab", "close", "resume", "pause",
];
impl<'a> Step<'a> {
    fn parse(value: &'a Value) -> Result<Self> {
        let map = value.as_object().ok_or_else(invalid)?;
        let mut verbs = VERBS.iter().filter(|verb| map.contains_key(**verb));
        let verb = verbs.next().ok_or_else(invalid)?;
        if verbs.next().is_some() {
            return Err(invalid());
        }
        let extra: &[&str] = match *verb {
            "fill" => &["text"],
            "select" => &["value"],
            "check" => &["on"],
            "press" => &["on"],
            "scroll" => &["in"],
            "drag" => &["to"],
            "upload" => &["grant"],
            _ => &[],
        };
        if map
            .keys()
            .any(|key| key != verb && !extra.contains(&key.as_str()))
        {
            return Err(invalid());
        }
        let step = Self { verb, map };
        // Everything a step needs is checked before the first one runs.
        step.targets()?;
        let text = |key: &str| map.get(key).and_then(Value::as_str).ok_or_else(invalid);
        match *verb {
            "open" => drop(text("open")?),
            "fill" => drop(text("text")?),
            "select" => drop(text("value")?),
            "press" => drop(text("press")?),
            "upload" => drop(text("grant")?),
            "tab" => drop(text("tab")?),
            "check" => {
                if map.get("on").is_some_and(|on| !on.is_boolean()) {
                    return Err(invalid());
                }
            }
            "scroll" => {
                map["scroll"].as_f64().ok_or_else(invalid)?;
            }
            "dialog" => {
                if !matches!(map["dialog"].as_str(), Some("accept" | "dismiss")) {
                    return Err(invalid());
                }
            }
            "wait" => {
                let wait = map["wait"].as_object().ok_or_else(invalid)?;
                if wait.len() != 1
                    || !["text", "url"]
                        .iter()
                        .any(|key| wait.get(*key).is_some_and(Value::is_string))
                {
                    return Err(invalid());
                }
            }
            "close" | "resume" | "pause" => {
                if map[*verb] != true {
                    return Err(invalid());
                }
            }
            _ => {}
        }
        Ok(step)
    }
    fn targets(&self) -> Result<Vec<Target>> {
        let keys: &[&str] = match self.verb {
            "click" | "hover" | "fill" | "select" | "check" | "upload" => &[self.verb],
            "press" => &["on"],
            "drag" => &["drag", "to"],
            "scroll" if self.map.contains_key("in") => &["in"],
            _ => &[],
        };
        keys.iter()
            .map(|key| Target::parse(self.map.get(*key).ok_or_else(invalid)?))
            .collect()
    }
    /// Whether the step can change the page, so that a view of the change is
    /// worth asking for.
    fn acts(&self) -> bool {
        matches!(
            self.verb,
            "click" | "hover" | "fill" | "select" | "check" | "press" | "scroll" | "drag"
        )
    }
    /// Whether the step leaves the page other than it was without a view of
    /// its own to show for it.
    fn changes(&self) -> bool {
        self.acts() || matches!(self.verb, "open" | "tab" | "dialog" | "upload" | "wait")
    }
    fn action(&self, references: &[String]) -> Value {
        let first = references.first().cloned().unwrap_or_default();
        match self.verb {
            "click" | "hover" => json!({"kind":self.verb,"ref":first}),
            "fill" => json!({"kind":"fill","ref":first,"text":self.map["text"]}),
            "select" => json!({"kind":"select","ref":first,"value":self.map["value"]}),
            "check" => json!({"kind":"check","ref":first,
                "checked":self.map.get("on").and_then(Value::as_bool).unwrap_or(true)}),
            "press" => json!({"kind":"press","ref":first,"key":self.map["press"]}),
            "drag" => json!({"kind":"drag","ref":first,"target_ref":references[1]}),
            "upload" => {
                json!({"kind":"attach_file","ref":first,"file_grant_id":self.map["grant"]})
            }
            "scroll" if references.is_empty() => {
                json!({"kind":"scroll","delta_y":self.map["scroll"]})
            }
            "scroll" => json!({"kind":"scroll","ref":first,"delta_y":self.map["scroll"]}),
            _ => Value::Null,
        }
    }
}

struct Call<'a> {
    browsers: &'a Browsers,
    principal: &'a Principal,
    deadline: Instant,
    operation_id: Option<&'a str>,
}
impl Call<'_> {
    fn slot(&self, id: &str) -> Result<Arc<Slot>> {
        self.browsers
            .v2_sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|slot| slot.owner == self.principal.owner)
            .cloned()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))
    }
    /// The session a call means: the one it names by the start of its ID, or
    /// the only one this caller has.
    fn session(&self, wanted: Option<&Value>) -> Result<Option<String>> {
        let wanted = match wanted {
            None => None,
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|text| {
                        (4..=64).contains(&text.len())
                            && text.bytes().all(|b| b.is_ascii_alphanumeric())
                    })
                    .ok_or_else(invalid)?,
            ),
        };
        let sessions = self.browsers.v2_sessions.lock().unwrap();
        let mut owned: Vec<&String> = sessions
            .iter()
            .filter(|(id, slot)| {
                slot.owner == self.principal.owner
                    && !slot.closing.load(Ordering::Acquire)
                    && wanted.is_none_or(|wanted| id.starts_with(wanted))
            })
            .map(|(id, _)| id)
            .collect();
        match (owned.len(), wanted) {
            (1, _) => Ok(owned.pop().cloned()),
            (0, None) => Ok(None),
            (0, Some(_)) => Err(Failure::new("browser_session_unavailable")),
            _ => Err(Failure::new("browser_session_ambiguous")),
        }
    }
    async fn inner(&self, id: &str, operation: &str, mut params: Value) -> Result<Value> {
        if self.slot(id)?.unsettled.load(Ordering::Acquire) {
            self.take_back(id).await;
        }
        params["session_id"] = id.into();
        if operation != "browser_tabs" {
            params["control_epoch"] = self.slot(id)?.control.snapshot().0.into();
        }
        validate(operation, &params)?;
        let result = self
            .browsers
            .v2_execute(
                operation,
                &params,
                self.principal,
                self.deadline,
                self.operation_id,
            )
            .await;
        if result.as_ref().is_err_and(|error| error.state == "unknown") {
            self.take_back(id).await;
        }
        result
    }
    /// A step whose outcome is unknown is reported as such; the session is not
    /// left paused for it. Every reference is gone by then, so the next step
    /// has to look at the page as it now is.
    async fn take_back(&self, id: &str) {
        let Ok(slot) = self.slot(id) else { return };
        let (epoch, paused) = slot.control.snapshot();
        if !paused {
            slot.unsettled.store(false, Ordering::Release);
            return;
        }
        // Whoever holds the session now, a person or a closing browser, the
        // pause is theirs and stays.
        if slot.human_held.load(Ordering::Acquire) || slot.closing.load(Ordering::Acquire) {
            slot.unsettled.store(false, Ordering::Release);
            return;
        }
        // Taking the session back waits for the input in flight, and that one
        // waits for its dialog. Leave it for a call that comes after the answer.
        if !slot
            .port()
            .is_ok_and(|port| port.connected() && port.pending() == 0)
            || self.dialog(id).is_some()
        {
            return;
        }
        let resumed = self
            .browsers
            .v2_control(
                id,
                &slot,
                &json!({"action":"resume","control_epoch":epoch}),
                self.deadline,
            )
            .await;
        // An input the browser has not answered yet keeps the session paused;
        // the next call tries again.
        slot.unsettled.store(resumed.is_err(), Ordering::Release);
    }
    /// Until when a step keeps trying while the page is busy.
    fn patience(&self) -> Instant {
        (Instant::now() + PATIENCE).min(
            self.deadline
                - Duration::from_millis(500)
                    .min(self.deadline.saturating_duration_since(Instant::now()) / 2),
        )
    }
    /// The references of a step's targets, all taken from one observation.
    async fn resolve(
        &self,
        id: &str,
        targets: &[Target],
    ) -> std::result::Result<(Vec<String>, Vec<String>), Stop> {
        let described: Vec<&Target> = targets
            .iter()
            .filter(|target| matches!(target, Target::Described { .. }))
            .collect();
        let found = if described.is_empty() {
            Vec::new()
        } else {
            // The search is narrowed by the words that say most: those of the
            // record around the target, or else its name.
            let mut query = json!({"max_nodes":1000,"max_chars":64000});
            if let [Target::Described { name, within, .. }] = described.as_slice() {
                query["find"] = within.as_ref().unwrap_or(name).clone().into();
            }
            let mut found = Vec::new();
            for _ in 0..SEARCH_PAGES {
                let read = self.inner(id, "browser_observe", query).await?;
                found.extend(candidates(&read));
                // Only the last page's references can be used, so a target on
                // an earlier page is looked for again by a narrower search.
                let Some(cursor) = read["next_cursor"].as_str() else {
                    break;
                };
                if targets.iter().all(|target| {
                    !matches!(target, Target::Described { .. }) || choose(target, &found).is_ok()
                }) {
                    break;
                }
                found.clear();
                query = json!({"cursor":cursor});
            }
            found
        };
        let mut references = Vec::new();
        let mut written = Vec::new();
        for target in targets {
            match target {
                Target::Reference(reference) => {
                    references.push(reference.clone());
                    written.push(target.written());
                }
                Target::Described { .. } => {
                    let chosen = &found[choose(target, &found)?];
                    references.push(chosen.reference.clone());
                    let mut text = format!("{} {:?}", chosen.role, chosen.name);
                    if let Target::Described {
                        within: Some(within),
                        ..
                    } = target
                    {
                        let _ = write!(text, " in {within:?}");
                    }
                    written.push(text);
                }
            }
        }
        Ok((references, written))
    }
    /// The dialog the browser is showing on the session's tab, if any.
    fn dialog(&self, id: &str) -> Option<sem::surface::Dialog> {
        let slot = self.slot(id).ok()?;
        let tab = slot.active.lock().unwrap().clone();
        slot.port().ok()?.for_tab(&tab).ok()?.dialog()
    }
    async fn tabs(&self, id: &str) -> Result<Value> {
        self.inner(id, "browser_tabs", json!({})).await
    }
    fn heading(tab: &Value) -> String {
        format!(
            "{} — {}",
            tab["title"].as_str().unwrap_or(""),
            tab["url"].as_str().unwrap_or("")
        )
    }
    /// The page as a full compact reading, under its title and address.
    async fn page(&self, id: &str, mut query: Value) -> Result<String> {
        query["format"] = "compact".into();
        let read = self.inner(id, "browser_observe", query).await?;
        let mut text = format!(
            "{} — {}\n{}",
            read["page"]["title"].as_str().unwrap_or(""),
            read["page"]["url"].as_str().unwrap_or(""),
            read["text"].as_str().unwrap_or("")
        );
        if let Some(cursor) = read["next_cursor"].as_str() {
            let _ = write!(text, "\nmore: {cursor}");
            if let Some(percent) = read["coverage"]["read_percent"].as_u64() {
                let _ = write!(text, " ({percent}% read)");
            }
        } else if read["coverage"]["complete"] == false {
            text.push_str("\n(part of the page was left out; narrow the read with \"find\")");
        }
        Ok(text)
    }
}

impl Browsers {
    pub(in crate::browser::runtime) async fn front(
        &self,
        operation: &str,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
        operation_id: Option<&str>,
    ) -> Result<Value> {
        let map = params.as_object().ok_or_else(invalid)?;
        let call = Call {
            browsers: self,
            principal,
            deadline,
            operation_id,
        };
        let allowed: &[&str] = match operation {
            "browser_do" => &["session", "steps", "read"],
            "browser_read" => &["session", "what", "find", "filter", "more", "max_chars"],
            _ => &["session"],
        };
        if map.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid());
        }
        let session = call.session(map.get("session"))?;
        match operation {
            "browser_do" => call.run(map, session).await,
            "browser_read" => call.read(map, session).await,
            "browser_see" => {
                let id = session.ok_or_else(|| Failure::new("browser_no_session"))?;
                call.inner(&id, "browser_screenshot", json!({})).await
            }
            _ => Err(Failure::new("browser_feature_unsupported")),
        }
    }
}

impl Call<'_> {
    async fn read(
        &self,
        map: &serde_json::Map<String, Value>,
        session: Option<String>,
    ) -> Result<Value> {
        let what = map.get("what").map_or(Some("page"), Value::as_str);
        if what == Some("sessions") {
            let mut text = String::new();
            for listed in self.browsers.v2_listing(self.principal, false) {
                let id = listed["session_id"].as_str().unwrap_or("");
                let _ = writeln!(
                    text,
                    "{} {} ({})",
                    &id[..id.len().min(6)],
                    listed["state"].as_str().unwrap_or(""),
                    if listed["controller"] == "ai" {
                        "yours"
                    } else {
                        "with a person"
                    }
                );
            }
            if text.is_empty() {
                text.push_str("no session is open");
            }
            return Ok(json!({"text":text.trim_end()}));
        }
        let id = session.ok_or_else(|| Failure::new("browser_no_session"))?;
        let text = match what {
            Some("tabs") => {
                let tabs = self.tabs(&id).await?;
                let mut text = String::new();
                for tab in tabs["tabs"].as_array().into_iter().flatten() {
                    let _ = writeln!(
                        text,
                        "{} {} {}",
                        if tab["tab_id"] == tabs["active_tab"] {
                            "*"
                        } else {
                            " "
                        },
                        tab["tab_id"].as_str().unwrap_or(""),
                        Self::heading(tab)
                    );
                }
                text.trim_end().to_owned()
            }
            Some("page") => {
                let mut query = json!({});
                for (from, to) in [
                    ("find", "find"),
                    ("filter", "filter"),
                    ("more", "cursor"),
                    ("max_chars", "max_chars"),
                ] {
                    if let Some(value) = map.get(from) {
                        query[to] = value.clone();
                    }
                }
                self.page(&id, query).await?
            }
            _ => return Err(invalid()),
        };
        Ok(json!({"text":text,"session_id":id}))
    }

    async fn run(
        &self,
        map: &serde_json::Map<String, Value>,
        mut session: Option<String>,
    ) -> Result<Value> {
        let steps = map
            .get("steps")
            .and_then(Value::as_array)
            .filter(|steps| (1..=MAX_STEPS).contains(&steps.len()))
            .ok_or_else(invalid)?;
        let steps: Vec<Step> = steps.iter().map(Step::parse).collect::<Result<_>>()?;
        let read = match map.get("read").map(Value::as_str) {
            None => "changes",
            Some(Some(read @ ("changes" | "page" | "none"))) => read,
            _ => return Err(invalid()),
        };
        let mut text = String::new();
        // Set when the latest step left the page other than it was and had no
        // view of its own to show for it, as after loading another document.
        let mut unseen = false;
        // The views of the steps, kept back until it is known whether the
        // whole page follows and makes them redundant.
        let mut views: Vec<(usize, String)> = Vec::new();
        let mut stopped = None;
        let mut done = 0;
        let last = steps.len() - 1;
        for (index, step) in steps.iter().enumerate() {
            let number = index + 1;
            let outcome = self.step(step, &mut session, read == "changes").await;
            match outcome {
                Ok((line, view, halt)) => {
                    let _ = writeln!(text, "{number} {line}");
                    if view["state"] == "sampled" {
                        let mut notes = String::new();
                        if view["settled"] == false {
                            notes.push_str(" (the page is still changing)");
                        }
                        if view["truncated"] == true {
                            notes.push_str(" (more changed than is shown)");
                        }
                        let mut body = format!("  changed{notes}:\n");
                        for line in view["text"].as_str().unwrap_or("").lines() {
                            let _ = writeln!(body, "  {line}");
                        }
                        views.push((text.len(), body));
                        unseen = view["truncated"] == true;
                    } else if view["reason"] == "no_visible_change" {
                        text.push_str("  (no visible change)\n");
                        unseen = false;
                    } else if step.changes() {
                        unseen = true;
                    }
                    done = number;
                    if halt {
                        if number <= last {
                            let _ = writeln!(text, "not run: {}–{}", number + 1, steps.len());
                        }
                        break;
                    }
                }
                Err(stop) => {
                    let _ = write!(text, "✗ {number} {}: {}", step.verb, stop.code);
                    let advice = advice(stop.code);
                    if !advice.is_empty() {
                        let _ = write!(text, " — {advice}");
                    }
                    if stop.state == "unknown" {
                        text.push_str(
                            " (it may or may not have taken effect; read the page to check)",
                        );
                    }
                    if !stop.detail.is_empty() {
                        let _ = write!(text, "\n{}", stop.detail);
                    }
                    text.push('\n');
                    if number <= last {
                        let _ = writeln!(text, "not run: {}–{}", number + 1, steps.len());
                    }
                    stopped = Some(stop);
                    break;
                }
            }
        }
        // Each step says what it changed where that could be told apart.
        // Where it could not, as after a navigation, the whole page follows.
        if !unseen {
            for (at, body) in views.iter().rev() {
                text.insert_str(*at, body);
            }
        }
        if (read == "page" || read == "changes" && unseen)
            && done > 0
            && let Some(id) = session.as_ref().filter(|id| self.slot(id).is_ok())
            && stopped.as_ref().is_none_or(|stop| stop.state != "unknown")
            // A page behind a dialog cannot be read until the dialog is answered.
            && self.dialog(id).is_none()
        {
            match self.page(id, json!({})).await {
                Ok(page) => {
                    let _ = write!(text, "--- page\n{page}");
                }
                Err(error) => {
                    let _ = write!(text, "--- the page cannot be read now: {}", error.code);
                }
            }
        }
        let mut value = json!({"text":text.trim_end(),"steps_done":done});
        if let Some(id) = session {
            value["session_id"] = id.into();
        }
        if let Some(stop) = stopped {
            value["failed"] = json!({"step":done + 1,"code":stop.code,"state":stop.state});
        }
        Ok(value)
    }

    /// Carries out one step. Returns its line of the reply, the view of what
    /// changed if one was asked for, and whether the rest must wait.
    async fn step(
        &self,
        step: &Step<'_>,
        session: &mut Option<String>,
        wants_view: bool,
    ) -> std::result::Result<(String, Value, bool), Stop> {
        let verb = step.verb;
        if verb == "open" {
            let url = step.map["open"].as_str().unwrap_or("");
            let id = match session.clone() {
                Some(id) => {
                    self.inner(
                        &id,
                        "browser_act",
                        json!({"action":{"kind":"navigate","url":url}}),
                    )
                    .await?;
                    id
                }
                None => {
                    let params = json!({"url":url,"selection":{"version":2,
                        "observation_profile":"semantic_dom_v1","backend":"cdp"}});
                    validate("browser_open", &params)?;
                    let selection =
                        crate::browser::selection::parse(&params)?.ok_or_else(invalid)?;
                    let opened = self
                        .browsers
                        .v2_open(&params, self.principal, selection, self.deadline)
                        .await?;
                    let id = opened["session_id"].as_str().unwrap_or("").to_owned();
                    *session = Some(id.clone());
                    id
                }
            };
            return Ok((
                format!("open {url} (session {})", &id[..6]),
                Value::Null,
                false,
            ));
        }
        let id = session
            .clone()
            .ok_or_else(|| Failure::new("browser_no_session"))?;
        // Nothing but an answer, or closing the session, gets past a dialog.
        if !matches!(verb, "dialog" | "close") && self.dialog(&id).is_some() {
            return Err(Failure::new("browser_dialog_open").into());
        }
        match verb {
            "close" => {
                self.inner(&id, "browser_control", json!({"action":"close"}))
                    .await?;
                *session = None;
                Ok(("close".into(), Value::Null, true))
            }
            "pause" | "resume" => {
                self.inner(&id, "browser_control", json!({"action":verb}))
                    .await?;
                Ok((verb.into(), Value::Null, verb == "pause"))
            }
            "wait" => {
                let wait = &step.map["wait"];
                let params = if let Some(text) = wait["text"].as_str() {
                    json!({"text_contains":text})
                } else {
                    json!({"url_contains":wait["url"]})
                };
                loop {
                    match self.inner(&id, "browser_wait", params.clone()).await {
                        // The page replaced its document under the read.
                        Err(error)
                            if passing(&Stop {
                                code: error.code,
                                state: error.state,
                                detail: String::new(),
                            }) && Instant::now() + Duration::from_secs(1) < self.deadline =>
                        {
                            tokio::time::sleep(RETRY).await;
                        }
                        other => {
                            other?;
                            break;
                        }
                    }
                }
                let line = match wait["text"].as_str() {
                    Some(text) => format!("wait for the text {text:?}"),
                    None => format!("wait for an address with {}", wait["url"]),
                };
                Ok((line, Value::Null, false))
            }
            "dialog" => {
                let dialog = self
                    .dialog(&id)
                    .ok_or_else(|| Failure::new("browser_dialog_unavailable"))?;
                let answer = step.map["dialog"].as_str().unwrap_or("");
                self.inner(
                    &id,
                    "browser_act",
                    json!({"action":{"kind":"dialog","accept":answer == "accept",
                        "dialog_id":dialog.dialog_id}}),
                )
                .await?;
                // The input that raised the dialog finishes now; the steps
                // after this one wait for it.
                let until = self.patience();
                while Instant::now() < until
                    && self.dialog(&id).is_none()
                    && self
                        .slot(&id)
                        .is_ok_and(|slot| slot.port().is_ok_and(|port| port.pending() != 0))
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if self.dialog(&id).is_none() {
                    self.take_back(&id).await;
                }
                Ok((
                    format!("dialog {answer} ({:?})", dialog.message),
                    Value::Null,
                    false,
                ))
            }
            "tab" => {
                let which = step.map["tab"].as_str().unwrap_or("");
                let mut params = json!({"action":{"kind":match which {
                    "new" => "new_tab",
                    "close" => "close_tab",
                    _ => "select_tab",
                }}});
                if !matches!(which, "new" | "close") {
                    params["tab_id"] = which.into();
                }
                let done = self.inner(&id, "browser_act", params).await?;
                Ok((
                    format!(
                        "tab {which} (now on {})",
                        done["tab_id"].as_str().unwrap_or("")
                    ),
                    Value::Null,
                    false,
                ))
            }
            _ => {
                let targets = step.targets()?;
                let described = targets
                    .iter()
                    .any(|target| matches!(target, Target::Described { .. }));
                let until = self.patience();
                let (mut done, written) = loop {
                    let attempt: std::result::Result<_, Stop> = async {
                        let (references, written) = self.resolve(&id, &targets).await?;
                        let mut params = json!({"action":step.action(&references)});
                        if wants_view && step.acts() {
                            params["after_action_view"] = true.into();
                        }
                        // A dialog the page raises holds the input open until it
                        // is answered. The step ends there: the answer is for the
                        // caller to choose, and it completes the input.
                        let act = self.inner(&id, "browser_act", params);
                        tokio::pin!(act);
                        let done = loop {
                            tokio::select! {
                                done = &mut act => match done {
                                    // The browser said so itself before the dialog was seen here.
                                    Err(error)
                                        if error.code == "browser_dialog_open"
                                            && self.dialog(&id).is_some() =>
                                    {
                                        if let Ok(slot) = self.slot(&id) {
                                            slot.unsettled.store(true, Ordering::Release);
                                        }
                                        break Value::Null;
                                    }
                                    other => break other?,
                                },
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                                    if self.dialog(&id).is_some() {
                                        // Leaving the input unfinished pauses the
                                        // session; answering the dialog lifts that.
                                        if let Ok(slot) = self.slot(&id) {
                                            slot.unsettled.store(true, Ordering::Release);
                                        }
                                        break Value::Null;
                                    }
                                }
                            }
                        };
                        Ok((done, written))
                    }
                    .await;
                    match attempt {
                        // A target given by description is looked up afresh, so a
                        // page that was still loading or redrawing gets another go.
                        Err(stop) if described && passing(&stop) && Instant::now() < until => {
                            tokio::time::sleep(RETRY).await;
                        }
                        other => break other?,
                    }
                };
                let mut line = if verb == "scroll" {
                    let by = &step.map["scroll"];
                    match written.first() {
                        Some(within) => format!("scroll {within} by {by}"),
                        None => format!("scroll by {by}"),
                    }
                } else {
                    format!("{verb} {}", written.join(" to "))
                };
                // A dialog the step raised keeps every later step from running.
                let dialog = self.dialog(&id);
                if let Some(dialog) = &dialog {
                    let _ = write!(
                        line,
                        "\n  a {} dialog opened: {:?} — answer it with {{\"dialog\":\"accept\"}} or {{\"dialog\":\"dismiss\"}}",
                        dialog.kind, dialog.message
                    );
                }
                Ok((line, done["after_action_view"].take(), dialog.is_some()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn all() -> Vec<Candidate> {
        [
            ("b1", "button", "Approve", "Order A1 Approve"),
            ("b2", "button", "Approve", "Order A2 Approve"),
            ("b3", "button", "approve all", ""),
            ("l1", "link", "Orders", ""),
        ]
        .into_iter()
        .map(|(reference, role, name, record)| Candidate {
            reference: reference.into(),
            role: role.into(),
            name: name.into(),
            record: record.into(),
        })
        .collect()
    }
    fn described(value: Value) -> Target {
        Target::parse(&value).unwrap()
    }
    #[test]
    fn a_description_stands_for_one_element_or_for_none() {
        let all = all();
        let pick =
            |value| choose(&described(value), &all).map(|index| all[index].reference.clone());
        assert_eq!(
            pick(json!({"name":"Approve","in":"Order A2"}))
                .ok()
                .unwrap(),
            "b2"
        );
        assert_eq!(pick(json!({"name":"Orders"})).ok().unwrap(), "l1");
        // The exact name wins over a longer name that contains it.
        assert_eq!(pick(json!({"name":"approve all"})).ok().unwrap(), "b3");
        assert_eq!(pick(json!({"name":"Approve","nth":2})).ok().unwrap(), "b2");
        assert_eq!(
            pick(json!({"name":"orders","role":"LINK"})).ok().unwrap(),
            "l1"
        );
        let ambiguous = pick(json!({"name":"Approve"})).err().unwrap();
        assert_eq!(ambiguous.code, "browser_target_ambiguous");
        assert!(
            ambiguous.detail.contains("in \"Order A1\"")
                && ambiguous.detail.contains("in \"Order A2\"")
        );
        assert!(!ambiguous.detail.contains("approve all"));
        for missing in [
            json!({"name":"Reject"}),
            json!({"name":"Approve","role":"link"}),
            json!({"name":"Approve","in":"Order A9"}),
        ] {
            assert_eq!(
                pick(missing).err().unwrap().code,
                "browser_target_not_found"
            );
        }
        assert_eq!(
            pick(json!({"name":"Approve","nth":3})).err().unwrap().code,
            "browser_target_ambiguous"
        );
    }
    #[test]
    fn a_step_names_one_verb_and_only_what_that_verb_takes() {
        for valid in [
            json!({"open":"https://example.com/"}),
            json!({"click":"r1"}),
            json!({"click":{"name":"Save","in":"Draft"}}),
            json!({"fill":{"name":"Email"},"text":"a@b.c"}),
            json!({"press":"Enter","on":"r2"}),
            json!({"check":"r3","on":false}),
            json!({"scroll":400}),
            json!({"scroll":-200,"in":"r4"}),
            json!({"drag":"r1","to":{"name":"Done"}}),
            json!({"wait":{"text":"Saved"}}),
            json!({"dialog":"accept"}),
            json!({"tab":"new"}),
            json!({"close":true}),
        ] {
            assert!(Step::parse(&valid).is_ok(), "{valid}");
        }
        for invalid in [
            json!({}),
            json!({"click":"r1","fill":"r2","text":"x"}),
            json!({"click":"r1","text":"x"}),
            json!({"click":{"role":"button"}}),
            json!({"click":{"name":"Save","near":"x"}}),
            json!({"fill":"r1"}),
            json!({"press":"Enter"}),
            json!({"dialog":"maybe"}),
            json!({"wait":{"text":"a","url":"b"}}),
            json!({"scroll":"down"}),
            json!({"close":false}),
        ] {
            assert!(Step::parse(&invalid).is_err(), "{invalid}");
        }
    }
}
