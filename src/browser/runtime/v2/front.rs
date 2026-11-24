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
/// Problems one step reports in full; the rest are counted.
const PROBLEMS: usize = 5;
/// How long a tab that a step opened is waited for before it can be worked in.
const NEW_TAB: Duration = Duration::from_secs(2);
/// Elements one reading of styles describes.
const STYLED: usize = 3;
/// Computed values an element has unless something sets them.
const USUAL: [(&str, &str); 14] = [
    ("visibility", "visible"),
    ("opacity", "1"),
    ("pointer-events", "auto"),
    ("position", "static"),
    ("z-index", "auto"),
    ("transform", "none"),
    ("line-height", "normal"),
    ("font-weight", "400"),
    ("text-align", "start"),
    ("cursor", "auto"),
    ("margin", "0px"),
    ("padding", "0px"),
    ("border-radius", "0px"),
    ("box-shadow", "none"),
];
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
            // A role alone says enough where the page has one element of it.
            name: match (text("name")?, map.contains_key("role")) {
                (Some(name), _) => name,
                (None, true) if !map.contains_key("name") => String::new(),
                _ => return Err(invalid()),
            },
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
            "what that reference stood for cannot be found; describe the target by name or read the page again"
        }
        "browser_not_actionable" => {
            "the element is hidden, disabled or covered right now; browser_read with what \"styles\" and find says what covers it"
        }
        "browser_dialog_open" => {
            "a dialog is open; answer it with {\"dialog\":\"accept\"} or {\"dialog\":\"dismiss\"}"
        }
        "browser_paused" => {
            "the browser is with a person or was paused; {\"resume\":true} takes it back once they are done"
        }
        "browser_sensitive_input" => {
            "this field takes a secret that only a person may type; {\"pause\":true} hands the browser to them"
        }
        "browser_dialog_unavailable" => "no dialog is open",
        "browser_value_rejected" => {
            "this field does not take that value: a date is YYYY-MM-DD, a time HH:MM, a colour #rrggbb, a slider a number within its range"
        }
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

/// What a reference stood for when it was given out: the kind of element,
/// its name, and the text of the row or card it was in. A reference the page
/// has outlived is looked for again by these, and only an element that
/// matches all three takes its place.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Meaning {
    role: String,
    name: String,
    context: String,
}
impl Meaning {
    /// Reads one node of a structured observation.
    pub(super) fn of(node: &Value) -> Self {
        Self {
            role: node["role"]
                .as_str()
                .filter(|role| !role.is_empty())
                .or(node["tag"].as_str())
                .unwrap_or("")
                .to_owned(),
            name: node["name"].as_str().unwrap_or("").trim().to_owned(),
            // Values are left out: what a field holds changes while it stays
            // the same field.
            context: node["record"]["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["text"].as_str())
                .flat_map(str::split_whitespace)
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// The meanings of the references a session has given out, the oldest
/// forgotten first.
#[derive(Default)]
pub(super) struct Meanings {
    known: HashMap<String, (String, Meaning)>,
    order: std::collections::VecDeque<String>,
}
impl Meanings {
    const MOST: usize = 4000;
    pub(super) fn keep(&mut self, tab: &str, issued: Vec<(String, Meaning)>) {
        for (reference, meaning) in issued {
            if self
                .known
                .insert(reference.clone(), (tab.to_owned(), meaning))
                .is_none()
            {
                self.order.push_back(reference);
            }
            if self.order.len() > Self::MOST
                && let Some(oldest) = self.order.pop_front()
            {
                self.known.remove(&oldest);
            }
        }
    }
    fn of(&self, reference: &str, tab: &str) -> Option<Meaning> {
        self.known
            .get(reference)
            .filter(|(of, _)| of == tab)
            .map(|(_, meaning)| meaning.clone())
    }
}

/// One element of a structured observation, as far as matching needs it.
struct Candidate {
    reference: String,
    role: String,
    name: String,
    record: String,
    meaning: Meaning,
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
                meaning: Meaning::of(node),
                reference: node["ref"].as_str()?.to_owned(),
                role: node["role"]
                    .as_str()
                    .filter(|role| !role.is_empty())
                    .or(node["tag"].as_str())
                    .unwrap_or("")
                    .to_owned(),
                // A field nothing names is known by what it holds.
                name: [node["name"].as_str(), node["value"].as_str()]
                    .into_iter()
                    .flatten()
                    .map(str::trim)
                    .find(|text| !text.is_empty())
                    .unwrap_or("")
                    .to_owned(),
                record,
            })
        })
        .collect()
}
/// The one element a description stands for. Several equally good matches are
/// listed for the caller to choose from; none is picked on its behalf.
/// The roles a verb acts on. Among elements that match a description equally
/// well, those are meant: a box to tick, not the form around it.
fn taking(verb: &str) -> &'static [&'static str] {
    match verb {
        "check" => &[
            "checkbox",
            "radio",
            "switch",
            "menuitemcheckbox",
            "menuitemradio",
        ],
        "select" => &["combobox", "listbox"],
        "fill" => &["textbox", "searchbox", "spinbutton", "combobox", "slider"],
        _ => &[],
    }
}
fn choose(target: &Target, all: &[Candidate], taking: &[&str]) -> std::result::Result<usize, Stop> {
    chosen(target, all, taking, false)
}
/// The elements a description matches best, in the order of the page.
fn matching(target: &Target, all: &[Candidate], taking: &[&str]) -> Vec<usize> {
    match chosen(target, all, taking, true) {
        Err(stop) if stop.code == "matches" => stop
            .detail
            .split(' ')
            .filter_map(|index| index.parse().ok())
            .collect(),
        _ => Vec::new(),
    }
}
fn chosen(
    target: &Target,
    all: &[Candidate],
    taking: &[&str],
    only_matches: bool,
) -> std::result::Result<usize, Stop> {
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
    let narrowed = |grade: &dyn Fn(&Candidate) -> bool| {
        let found: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, candidate)| fits(candidate) && grade(candidate))
            .map(|(index, _)| index)
            .collect();
        let found = if found
            .iter()
            .any(|index| taking.contains(&all[*index].role.as_str()))
        {
            found
                .into_iter()
                .filter(|index| taking.contains(&all[*index].role.as_str()))
                .collect()
        } else {
            found
        };
        // An element that only looks as if it took clicks gives way to a
        // control of the same name: it is that control's wrapping or inside.
        if found.iter().any(|index| all[*index].role != "inferred") {
            found
                .into_iter()
                .filter(|index| all[*index].role != "inferred")
                .collect::<Vec<usize>>()
        } else {
            found
        }
    };
    for grade in grades {
        let found = narrowed(grade);
        if only_matches {
            if found.is_empty() {
                continue;
            }
            return Err(Stop {
                code: "matches",
                state: "not_sent",
                detail: found
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            });
        }
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

/// The element that now is what a reference stood for. One that is not there,
/// or that cannot be told from another, is not replaced by a guess.
fn recall(meaning: &Meaning, all: &[Candidate]) -> std::result::Result<usize, Stop> {
    let mut same = all
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.meaning == *meaning);
    match (same.next(), same.next()) {
        (Some((index, _)), None) => Ok(index),
        (found, _) => Err(Stop {
            code: "browser_stale_reference",
            state: "not_sent",
            detail: format!(
                "it stood for {} {:?}{}, which {}",
                meaning.role,
                meaning.name,
                if meaning.context.is_empty() {
                    String::new()
                } else {
                    format!(
                        " in {:?}",
                        meaning.context.chars().take(80).collect::<String>()
                    )
                },
                if found.is_some() {
                    "several elements now are"
                } else {
                    "is no longer on the page"
                }
            ),
        }),
    }
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
            "click" => &["button", "count"],
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
            "click" => {
                // One press of the main button unless the step says otherwise;
                // a double click with the other button is not a thing pages hear.
                let right = match map.get("button").map(Value::as_str) {
                    None | Some(Some("left")) => false,
                    Some(Some("right")) => true,
                    _ => return Err(invalid()),
                };
                let twice = match map.get("count").map(Value::as_u64) {
                    None | Some(Some(1)) => false,
                    Some(Some(2)) => true,
                    _ => return Err(invalid()),
                };
                if right && twice {
                    return Err(invalid());
                }
            }
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
            "click" if self.map.get("button") == Some(&json!("right")) => {
                json!({"kind":"click","ref":first,"manner":"right"})
            }
            "click" if self.map.get("count") == Some(&json!(2)) => {
                json!({"kind":"click","ref":first,"manner":"double"})
            }
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
    /// The tab a reading is of, when it is not the one being worked on.
    tab: Option<String>,
}

/// The paints, layout shifts and long tasks a page measured, as lines.
fn vitals(read: &Value) -> String {
    use sem::task_events::journal::span;
    let after = |at: &Value| at.as_f64().map(|ms| span(ms.max(0.0).round() as u64));
    let named = |element: &Value| match (element["tag"].as_str(), element["name"].as_str()) {
        (Some(tag), Some(name)) if !name.is_empty() => format!(" ({tag} {name:?})"),
        (Some(tag), _) => format!(" ({tag})"),
        _ => String::new(),
    };
    let mut paint = Vec::new();
    if let Some(at) = after(&read["fcp"]) {
        paint.push(format!("first content after {at}"));
    }
    if let Some(at) = after(&read["lcp"]["at"]) {
        paint.push(format!(
            "largest content after {at}{}",
            named(&read["lcp"]["element"])
        ));
    }
    let mut text = if paint.is_empty() {
        "\npaint: nothing has been painted yet".to_owned()
    } else {
        format!("\npaint: {}", paint.join(", "))
    };
    let shifts = &read["shifts"];
    match (
        shifts["count"].as_u64().unwrap_or(0),
        after(&shifts["largest"]["at"]),
    ) {
        (count @ 1.., Some(at)) => {
            let _ = write!(
                text,
                "\nlayout shift: {:.2} in {count}, the largest {:.2} after {at}{}",
                shifts["score"].as_f64().unwrap_or(0.0),
                shifts["largest"]["value"].as_f64().unwrap_or(0.0),
                named(&shifts["largest"]["element"])
            );
        }
        _ => text.push_str("\nlayout shift: none"),
    }
    let tasks = &read["tasks"];
    match tasks["count"].as_u64() {
        None => text.push_str("\nlong tasks: not measured by this browser"),
        Some(0) => text.push_str("\nlong tasks: none"),
        Some(count) => {
            let whole = |value: &Value| span(value.as_f64().unwrap_or(0.0).max(0.0).round() as u64);
            let _ = write!(
                text,
                "\nlong tasks: {count} totalling {}, the longest {} after {}",
                whole(&tasks["total"]),
                whole(&tasks["longest"]),
                whole(&tasks["at"])
            );
        }
    }
    text
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
            // Asks an open step for a session beside the ones there are.
            Some(value) if value == "new" => return Ok(None),
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
        if let Some(tab) = &self.tab
            && !matches!(operation, "browser_tabs" | "browser_control")
            && params.get("tab_id").is_none()
        {
            params["tab_id"] = tab.clone().into();
        }
        // A tab is named by the start of its ID, as it is written in replies.
        if let Some(tab) = params["tab_id"].as_str() {
            params["tab_id"] = self.whole(id, tab).into();
        }
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
    /// The journal of the tab the session is on, begun now if there is none,
    /// and whether it began with this call. A journal ends when control
    /// changes hands, so that nothing a person did is in the next one.
    async fn journal(&self, id: &str) -> Option<(sem::task_events::Sink, bool)> {
        let slot = self.slot(id).ok()?;
        let tab = slot.active.lock().unwrap().clone();
        if let Some((of, sink)) = &*slot.journal.lock().unwrap()
            && *of == tab
            && sink.active()
        {
            return Some((sink.clone(), false));
        }
        let (epoch, paused) = slot.control.snapshot();
        if paused || slot.closing.load(Ordering::Acquire) {
            return None;
        }
        let port = slot.port().ok()?.for_tab(&tab).ok()?;
        let page = port
            .tabs()
            .into_iter()
            .find(|listed| listed.tab_id == tab)
            .map(|listed| listed.url)
            .unwrap_or_default();
        let sink = sem::task_events::Sink::journal(slot.control.clone(), epoch, &page);
        let until = self.deadline.min(Instant::now() + Duration::from_secs(3));
        port.enable_task_events(sink.clone(), until).await.ok()?;
        *slot.journal.lock().unwrap() = Some((tab, sink.clone()));
        Some((sink, true))
    }
    /// Fetches what the failed requests were answered with, while the browser
    /// still has it.
    async fn bodies(&self, id: &str) {
        let Ok(slot) = self.slot(id) else { return };
        let Some((tab, sink)) = slot.journal.lock().unwrap().clone() else {
            return;
        };
        let wanted = sink
            .journal_read(|log| log.wanting(PROBLEMS))
            .unwrap_or_default();
        if wanted.is_empty() {
            return;
        }
        let Some(port) = slot.port().ok().and_then(|port| port.for_tab(&tab).ok()) else {
            return;
        };
        for (entry, request) in wanted {
            let until = self
                .deadline
                .min(Instant::now() + Duration::from_millis(500));
            if let Ok(reply) = port.response_body(&request, until).await {
                sink.journal_read(|log| log.body(entry, &reply));
            }
        }
    }
    /// The errors and failed requests the page has had since they were last
    /// reported, as lines under a step.
    fn problems(&self, id: &str, indent: &str) -> String {
        let Ok(slot) = self.slot(id) else {
            return String::new();
        };
        let sink = slot
            .journal
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, sink)| sink.clone());
        let Some((lines, more)) =
            sink.and_then(|sink| sink.journal_read(|log| log.problems(PROBLEMS)))
        else {
            return String::new();
        };
        if lines.is_empty() {
            return String::new();
        }
        let mut text = format!("{indent}problems:\n");
        for line in lines {
            let _ = writeln!(text, "{indent}  {line}");
        }
        if more > 0 {
            let _ = writeln!(
                text,
                "{indent}  +{more} more; browser_read with what \"console\" or \"network\" lists them"
            );
        }
        text
    }
    /// How the elements with a text are laid out and drawn, and whether a
    /// click at their centre would reach them.
    async fn styles(&self, id: &str, find: &str) -> Result<String> {
        let tab = self.slot(id)?.active.lock().unwrap().clone();
        let evidence = |query: Value| {
            let params = json!({"version":1,"tab_id":tab,"query":query});
            async move { self.inner(id, "browser_evidence", params).await }
        };
        let found = evidence(json!({"kind":"find_nodes","text":find,"limit":STYLED + 1})).await?;
        let nodes = found["nodes"].as_array().map_or(&[][..], Vec::as_slice);
        if nodes.is_empty() {
            return Ok(format!(
                "styles: no element has the text {find:?}; the text is matched with its case"
            ));
        }
        let mut text = format!(
            "styles: {} with the text {find:?}",
            match nodes.len() {
                1 => "1 element".to_owned(),
                count if count > STYLED => format!("the first {STYLED} elements"),
                count => format!("{count} elements"),
            }
        );
        let named = |node: &Value| {
            format!(
                "{} {:?}",
                node["tag"].as_str().unwrap_or("?"),
                node["name"].as_str().unwrap_or("")
            )
        };
        for node in nodes.iter().take(STYLED) {
            let _ = write!(text, "\n{}", named(node));
            let read = match evidence(json!({"kind":"read_layout","node_id":node["node_id"]})).await
            {
                Ok(read) => read,
                // The page replaced the element between the two reads.
                Err(error) if error.code == "browser_stale_reference" => {
                    text.push_str(" — no longer on the page");
                    continue;
                }
                Err(error) => return Err(error),
            };
            let layout = &read["layout"];
            let number = |value: &Value| value.as_f64().unwrap_or(0.0).round();
            let (width, height) = (
                number(&layout["box"]["width"]),
                number(&layout["box"]["height"]),
            );
            let _ = write!(
                text,
                " — {width}×{height} at {},{} in a {}×{} viewport; ",
                number(&layout["box"]["x"]),
                number(&layout["box"]["y"]),
                number(&layout["viewport"]["width"]),
                number(&layout["viewport"]["height"])
            );
            match layout["hit"]["kind"].as_str() {
                Some("target" | "descendant") => text.push_str("on top at its centre"),
                Some("outside_viewport") => text.push_str("its centre is outside the viewport"),
                Some("other") => {
                    let cover =
                        evidence(json!({"kind":"read_node","node_id":layout["hit"]["node_id"]}))
                            .await;
                    match cover {
                        Ok(cover) => {
                            let _ =
                                write!(text, "covered at its centre by {}", named(&cover["node"]));
                        }
                        Err(_) => text.push_str("covered at its centre by another element"),
                    }
                }
                _ if width == 0.0 || height == 0.0 => text.push_str("it takes up no space"),
                _ => text.push_str("what is on top of it could not be told"),
            }
            let computed = &layout["computed"];
            let mut said = Vec::new();
            for (name, value) in computed.as_object().into_iter().flatten() {
                let value = value.as_str().unwrap_or("");
                // The size is given above, and a value every element starts
                // with says nothing about this one.
                if value.is_empty()
                    || matches!(name.as_str(), "width" | "height" | "overflow-y")
                    || USUAL.contains(&(name.as_str(), value))
                    || name == "border" && value.starts_with("0px")
                {
                    continue;
                }
                if name == "overflow-x" {
                    let other = computed["overflow-y"].as_str().unwrap_or(value);
                    if (value, other) != ("visible", "visible") {
                        said.push(if value == other {
                            format!("overflow: {value}")
                        } else {
                            format!("overflow: {value} {other}")
                        });
                    }
                    continue;
                }
                said.push(format!("{name}: {value}"));
            }
            if !said.is_empty() {
                let _ = write!(text, "\n  {}", said.join("; "));
            }
        }
        Ok(text)
    }
    /// Until when a step keeps trying while the page is busy.
    fn patience(&self) -> Instant {
        (Instant::now() + PATIENCE).min(
            self.deadline
                - Duration::from_millis(500)
                    .min(self.deadline.saturating_duration_since(Instant::now()) / 2),
        )
    }
    /// Matches that read alike are told apart by where they are: each is given
    /// its place among them, the heading it comes under, and whether it is in
    /// view. The page is read once more for that, so it is done only when the
    /// lines would otherwise be the same.
    async fn told_apart(
        &self,
        id: &str,
        target: &Target,
        all: &[Candidate],
        taking: &[&str],
        stop: Stop,
    ) -> Stop {
        let found = matching(target, all, taking);
        let lines: Vec<String> = found
            .iter()
            .take(MAX_CANDIDATES)
            .map(|index| all[*index].line())
            .collect();
        let distinct: std::collections::HashSet<&String> = lines.iter().collect();
        if distinct.len() == lines.len() {
            return stop;
        }
        let Ok(read) = self
            .inner(
                id,
                "browser_observe",
                json!({"filter":"interactive","format":"search","max_nodes":1000,"max_chars":64000}),
            )
            .await
        else {
            return stop;
        };
        // A button that is all its heading says comes under the heading before.
        let mut heading = String::new();
        let mut earlier = String::new();
        let mut places = Vec::new();
        for node in read["nodes"].as_array().into_iter().flatten() {
            let role = node["role"]
                .as_str()
                .filter(|role| !role.is_empty())
                .or(node["tag"].as_str())
                .unwrap_or("");
            let name = [node["name"].as_str(), node["value"].as_str()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|text| !text.is_empty())
                .unwrap_or("");
            if role == "heading" {
                if !found.iter().any(|index| all[*index].name == name) {
                    earlier = name.chars().take(60).collect();
                }
                heading = name.chars().take(60).collect();
            } else if node["ref"].is_string()
                && found
                    .iter()
                    .any(|index| all[*index].role == role && all[*index].name == name)
            {
                let under = if heading == name { &earlier } else { &heading };
                places.push((under.clone(), node["in_view"] == true));
            }
        }
        if read["next_cursor"].is_string() || places.len() != found.len() {
            return stop;
        }
        let mut detail = format!("{} match", found.len());
        for (place, (line, (heading, seen))) in lines.iter().zip(&places).enumerate() {
            let _ = write!(detail, "\n  nth {}: {}", place + 1, line.trim_start());
            if !heading.is_empty() {
                let _ = write!(detail, " under {heading:?}");
            }
            if !seen {
                detail.push_str(" (offscreen)");
            }
        }
        if found.len() > MAX_CANDIDATES {
            let _ = write!(detail, "\n  … and {} more", found.len() - MAX_CANDIDATES);
        }
        Stop { detail, ..stop }
    }
    /// The references of a step's targets, all taken from one observation.
    /// With `again`, a reference is not used as it is but looked for by what
    /// it stood for: the page has redrawn since it was given out.
    async fn resolve(
        &self,
        id: &str,
        targets: &[Target],
        again: bool,
        verb: &str,
    ) -> std::result::Result<(Vec<String>, Vec<String>), Stop> {
        let taking = taking(verb);
        let tab = self.active(id);
        let meant = |target: &Target| match target {
            Target::Reference(reference) if again => self
                .slot(id)
                .ok()
                .and_then(|slot| slot.meanings.lock().unwrap().of(reference, &tab)),
            _ => None,
        };
        let described: Vec<&Target> = targets
            .iter()
            .filter(|target| matches!(target, Target::Described { .. }))
            .collect();
        let recalled: Vec<Meaning> = targets.iter().filter_map(meant).collect();
        let found = if described.is_empty() && recalled.is_empty() {
            Vec::new()
        } else {
            // The search is narrowed by the words that say most: those of the
            // record around the target, or else its name.
            let mut query = json!({"max_nodes":1000,"max_chars":64000,"format":"search"});
            if verb == "drag" {
                // What can be dragged, and where it can be dropped, is listed
                // apart from the controls, and only such an element is taken.
                query["filter"] = "drag_endpoints".into();
            } else if let ([Target::Described { name, within, .. }], []) =
                (described.as_slice(), recalled.as_slice())
            {
                let words = within.as_ref().unwrap_or(name);
                if !words.is_empty() {
                    query["find"] = words.clone().into();
                } else if matches!(described[0], Target::Described { role: Some(role), .. } if role != "text")
                {
                    // Asked for by its role alone, it is looked for among the
                    // controls: the text of a long page would use up the
                    // search before the control is reached.
                    query["filter"] = "interactive".into();
                }
            } else if let ([], [meaning]) = (described.as_slice(), recalled.as_slice())
                && !meaning.name.is_empty()
                && meaning.name.chars().count() <= 128
            {
                query["find"] = meaning.name.clone().into();
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
                if targets.iter().all(|target| match (target, meant(target)) {
                    (_, Some(meaning)) => recall(&meaning, &found).is_ok(),
                    (Target::Described { .. }, _) => choose(target, &found, taking).is_ok(),
                    _ => true,
                }) {
                    break;
                }
                found.clear();
                query = json!({"cursor":cursor,"format":"search"});
            }
            found
        };
        let mut references = Vec::new();
        let mut written = Vec::new();
        for target in targets {
            match target {
                Target::Reference(reference) => match meant(target) {
                    Some(meaning) => {
                        let chosen = &found[recall(&meaning, &found)?];
                        references.push(chosen.reference.clone());
                        written.push(format!(
                            "{} {:?} (was [{reference}], now [{}])",
                            chosen.role, chosen.name, chosen.reference
                        ));
                    }
                    None => {
                        references.push(reference.clone());
                        written.push(target.written());
                    }
                },
                Target::Described {
                    name,
                    role,
                    within,
                    nth,
                } => {
                    // Words that name no control may still be something that takes
                    // clicks. The search for them replaces the references found so
                    // far, so it is made only for a step with a single target.
                    // The role "text" asks for the words themselves, where a
                    // control is called the same.
                    let words = role.as_deref() == Some("text");
                    let worded;
                    let picked = match choose(target, &found, taking) {
                        Err(stop)
                            if stop.code == "browser_target_ambiguous" && targets.len() == 1 =>
                        {
                            Err(self.told_apart(id, target, &found, taking, stop).await)
                        }
                        other => other,
                    };
                    let chosen = match picked {
                        Err(stop)
                            if stop.code == "browser_target_not_found"
                                && targets.len() == 1
                                && (role.is_none() || words)
                                && !name.is_empty() =>
                        {
                            let read = self
                                .inner(
                                    id,
                                    "browser_observe",
                                    json!({"filter":"text","find":name,"format":"search",
                                        "max_nodes":1000,"max_chars":64000}),
                                )
                                .await?;
                            worded = candidates(&read)
                                .into_iter()
                                .filter(|candidate| !words || candidate.role == "inferred")
                                .collect::<Vec<_>>();
                            let plain = Target::Described {
                                name: name.clone(),
                                role: None,
                                within: within.clone(),
                                nth: *nth,
                            };
                            &worded[choose(&plain, &worded, taking)?]
                        }
                        other => &found[other?],
                    };
                    references.push(chosen.reference.clone());
                    let mut text = if chosen.name.is_empty() {
                        chosen.role.clone()
                    } else {
                        format!("{} {:?}", chosen.role, chosen.name)
                    };
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
    /// The tabs of a session, one to a line, the one worked on marked.
    async fn listed(&self, id: &str) -> Result<String> {
        let tabs = self.tabs(id).await?;
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
                self.brief(id, tab["tab_id"].as_str().unwrap_or("")),
                Self::heading(tab)
            );
        }
        Ok(text)
    }
    fn active(&self, id: &str) -> String {
        self.slot(id)
            .map(|slot| slot.active.lock().unwrap().clone())
            .unwrap_or_default()
    }
    /// The tab an ID or the start of one stands for. What fits no single tab
    /// is passed on as it is, to be refused where tabs are known.
    fn whole(&self, id: &str, tab: &str) -> String {
        let ids = self.tab_ids(id);
        let mut fitting = ids.iter().filter(|known| known.starts_with(tab));
        match (fitting.next(), fitting.next()) {
            (Some(only), None) => only.clone(),
            _ => tab.to_owned(),
        }
    }
    /// A tab's ID cut to what tells it from the session's other tabs.
    fn brief(&self, id: &str, tab: &str) -> String {
        let ids = self.tab_ids(id);
        let mut length = 6.min(tab.len());
        while length < tab.len()
            && ids
                .iter()
                .any(|other| other != tab && other.starts_with(&tab[..length]))
        {
            length += 1;
        }
        tab.get(..length).unwrap_or(tab).to_owned()
    }
    fn tab_ids(&self, id: &str) -> Vec<String> {
        self.slot(id)
            .and_then(|slot| slot.port())
            .map(|port| port.tabs().into_iter().map(|tab| tab.tab_id).collect())
            .unwrap_or_default()
    }
    /// A failed navigation with the reason the browser gave for it.
    fn why(&self, id: &str, error: Failure) -> Stop {
        let mut stop = Stop::from(error);
        if stop.code == "browser_navigation_failed"
            && let Ok(slot) = self.slot(id)
            && let Ok(port) = slot.port()
            && let Some(why) = sem::Port::coordination(&port).why_navigation_failed()
        {
            stop.detail = format!("  {why}");
        }
        stop
    }
    /// Makes a tab the one that is worked on.
    async fn move_to(&self, id: &str, tab: &str) -> Result<Value> {
        self.inner(
            id,
            "browser_act",
            json!({"action":{"kind":"select_tab"},"tab_id":tab}),
        )
        .await
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
        let tab = map
            .get("tab")
            .map(|tab| {
                tab.as_str()
                    .filter(|tab| {
                        (1..=128).contains(&tab.len())
                            && tab
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    })
                    .map(str::to_owned)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        let call = Call {
            browsers: self,
            principal,
            deadline,
            operation_id,
            // Steps are carried out in the tab that is worked on, so a call
            // that names another one moves there first.
            tab: tab.clone().filter(|_| operation != "browser_do"),
        };
        let allowed: &[&str] = match operation {
            "browser_do" => &["session", "tab", "steps", "read"],
            "browser_read" => &[
                "session",
                "tab",
                "what",
                "find",
                "filter",
                "more",
                "max_chars",
            ],
            _ => &["session", "tab"],
        };
        if map.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(invalid());
        }
        // Listing the sessions, or the tabs of all of them, is how a caller
        // with several learns their names.
        let all = operation == "browser_read"
            && (map.get("what") == Some(&json!("sessions"))
                || map.get("what") == Some(&json!("tabs")) && !map.contains_key("session"));
        let session = if all {
            None
        } else {
            call.session(map.get("session"))?
        };
        match operation {
            "browser_do" => {
                if let (Some(id), Some(tab)) = (&session, &tab)
                    && call.active(id) != *tab
                {
                    call.move_to(id, tab).await?;
                }
                call.run(map, session).await
            }
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
        if what == Some("tabs") && session.is_none() {
            let owned: Vec<String> = self
                .browsers
                .v2_listing(self.principal, false)
                .iter()
                .filter_map(|listed| listed["session_id"].as_str().map(str::to_owned))
                .collect();
            let mut text = String::new();
            match owned.as_slice() {
                [] => return Err(Failure::new("browser_no_session")),
                [only] => {
                    text = self.listed(only).await?;
                    return Ok(json!({"text":text.trim_end(),"session_id":only}));
                }
                several => {
                    for id in several {
                        let _ = writeln!(text, "session {}", &id[..id.len().min(6)]);
                        match self.listed(id).await {
                            Ok(tabs) => text.push_str(&tabs),
                            Err(error) => {
                                let _ = writeln!(text, "  ({})", error.code);
                            }
                        }
                    }
                }
            }
            return Ok(json!({"text":text.trim_end()}));
        }
        let id = session.ok_or_else(|| Failure::new("browser_no_session"))?;
        if matches!(what, Some("console" | "network" | "performance" | "styles"))
            && self
                .tab
                .as_ref()
                .is_some_and(|tab| self.whole(&id, tab) != self.active(&id))
        {
            return Ok(json!({"text":format!(
                "{}: only the tab that is worked on is recorded and measured; move to this one first with a {{\"tab\":…}} step",
                what.unwrap_or("")
            ),"session_id":id}));
        }
        let text = match what {
            Some("tabs") => self.listed(&id).await?.trim_end().to_owned(),
            Some(what @ ("console" | "network")) => {
                if map.contains_key("more") {
                    return Err(invalid());
                }
                let find = map
                    .get("find")
                    .map(|find| {
                        find.as_str()
                            .filter(|find| (1..=200).contains(&find.len()))
                            .ok_or_else(invalid)
                    })
                    .transpose()?;
                let narrow = if what == "console" {
                    "errors"
                } else {
                    "failed"
                };
                let narrowed = match map.get("filter") {
                    None => false,
                    Some(filter) if filter == narrow => true,
                    Some(_) => return Err(invalid()),
                };
                let budget = map
                    .get("max_chars")
                    .map(|most| {
                        most.as_u64()
                            .filter(|most| (200..=64_000).contains(most))
                            .ok_or_else(invalid)
                    })
                    .transpose()?
                    .unwrap_or(4000) as usize;
                let paused = || Failure::new("browser_paused");
                let (sink, fresh) = self.journal(&id).await.ok_or_else(|| {
                    if self.slot(&id).is_ok_and(|slot| !slot.control.snapshot().1) {
                        Failure::new("browser_busy")
                    } else {
                        paused()
                    }
                })?;
                self.bodies(&id).await;
                let entry = find.filter(|find| {
                    find.len() > 1
                        && find.starts_with(['c', 'n'])
                        && find[1..].bytes().all(|byte| byte.is_ascii_digit())
                });
                let mut text = sink
                    .journal_read(|log| match entry {
                        Some(entry) => log.detail(entry).unwrap_or_else(|| {
                            format!("{entry} is not kept any more; list the entries again")
                        }),
                        None if what == "console" => log.console(find, narrowed, budget),
                        None => log.network(find, narrowed, budget),
                    })
                    .ok_or_else(paused)?;
                if fresh {
                    text.push_str(
                        "\n(recording began with this call; what the page reported before it was not kept)",
                    );
                }
                text
            }
            Some("performance") => {
                if ["find", "filter", "more", "max_chars"]
                    .iter()
                    .any(|key| map.contains_key(*key))
                {
                    return Err(invalid());
                }
                let (sink, _) = self.journal(&id).await.ok_or_else(|| {
                    Failure::new(
                        if self.slot(&id).is_ok_and(|slot| !slot.control.snapshot().1) {
                            "browser_busy"
                        } else {
                            "browser_paused"
                        },
                    )
                })?;
                let mut text = sink
                    .journal_read(|log| log.performance())
                    .ok_or_else(|| Failure::new("browser_paused"))?;
                let measured = match self
                    .slot(&id)
                    .and_then(|slot| slot.port()?.for_tab(&self.active(&id)))
                {
                    Ok(port) => {
                        port.vitals(self.deadline.min(Instant::now() + Duration::from_secs(2)))
                            .await
                    }
                    Err(error) => Err(error),
                };
                match measured {
                    Ok(measured) => text.push_str(&vitals(&measured)),
                    Err(_) => text.push_str(
                        "\npaint, layout shift and long tasks: the page could not be asked now",
                    ),
                }
                text
            }
            Some("styles") => {
                let find = map
                    .get("find")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                if map.contains_key("more") || map.contains_key("filter") {
                    return Err(invalid());
                }
                self.styles(&id, find).await?
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
                // A page is not read in less than this; a smaller wish gets
                // the smallest reading there is instead of a refusal.
                if let Some(wished) = query["max_chars"].as_u64() {
                    query["max_chars"] = wished.max(1024).into();
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
        // What the page reported between two calls is none of these steps' doing.
        if let Some(id) = &session {
            self.journal(id).await;
            self.bodies(id).await;
            // A request whose answer has not ended is kept back once, for
            // its answer to be told with it. Here nothing more is awaited:
            // what was kept back is asked for again and told as it is.
            let mut earlier = self.problems(id, "  ");
            let kept = self.problems(id, "  ");
            match kept.split_once('\n') {
                Some((_, lines)) if !earlier.is_empty() => earlier.push_str(lines),
                _ => earlier.push_str(&kept),
            }
            if !earlier.is_empty() {
                text.push_str("before these steps\n");
                text.push_str(&earlier);
            }
        }
        for (index, step) in steps.iter().enumerate() {
            let number = index + 1;
            let before = session
                .as_ref()
                .map(|id| self.tab_ids(id))
                .unwrap_or_default();
            // A page can close its own tab. The work goes on in one that is left.
            if let Some(id) = &session
                && let Some(left) = before.first()
                && !before.contains(&self.active(id))
                && !matches!(step.verb, "tab" | "close")
                && self.move_to(id, left).await.is_ok()
            {
                let _ = writeln!(
                    text,
                    "(the tab that was worked on has closed; now on {})",
                    self.brief(id, left)
                );
            }
            if let Some(id) = &session {
                self.journal(id).await;
            }
            let outcome = self.step(step, &mut session, read == "changes").await;
            // A step that opens a tab is followed there, as the browser's own
            // window is: what the step led to is in the new tab.
            let mut moved = String::new();
            if outcome.is_ok()
                && !before.is_empty()
                && !matches!(step.verb, "tab" | "close")
                && let Some(id) = &session
                && let Some(new) = self
                    .tab_ids(id)
                    .into_iter()
                    .rfind(|tab| !before.contains(tab))
            {
                let old = self.brief(id, &self.active(id));
                let until = (Instant::now() + NEW_TAB).min(self.patience());
                let arrived = loop {
                    match self.move_to(id, &new).await {
                        Ok(_) => break true,
                        // The tab is listed before it can be worked in.
                        Err(error) if error.state == "not_sent" && Instant::now() < until => {
                            tokio::time::sleep(RETRY).await;
                        }
                        Err(_) => break false,
                    }
                };
                moved = if arrived {
                    format!(
                        "  a new tab opened and is worked on now; go back with {{\"tab\":\"{old}\"}}\n"
                    )
                } else {
                    let new = self.brief(id, &new);
                    format!("  a new tab opened: {new}; move to it with {{\"tab\":\"{new}\"}}\n")
                };
            }
            let problems = match &session {
                Some(id) => {
                    self.bodies(id).await;
                    self.problems(id, "  ")
                }
                None => String::new(),
            };
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
                        let shown = view["text"].as_str().unwrap_or("");
                        let mut body = if shown.trim().is_empty() {
                            "  (something was removed from the page; nothing else changed)\n"
                                .to_owned()
                        } else {
                            format!("  changed{notes}:\n")
                        };
                        for line in shown.lines() {
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
                    if !moved.is_empty() {
                        text.push_str(&moved);
                        unseen = true;
                    }
                    text.push_str(&problems);
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
                    text.push_str(&problems);
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
        let before_page = text.len();
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
        // What came in while the page was read is not left for the next call.
        if let Some(id) = &session {
            self.bodies(id).await;
            text.insert_str(before_page, &self.problems(id, ""));
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
        // Nothing but an answer, or closing the session, gets past a dialog.
        if verb == "open"
            && let Some(id) = session.as_ref()
            && self.dialog(id).is_some()
        {
            return Err(Failure::new("browser_dialog_open").into());
        }
        if verb == "open" {
            let url = step.map["open"].as_str().unwrap_or("");
            let id = match session.clone() {
                Some(id) => {
                    self.inner(
                        &id,
                        "browser_act",
                        json!({"action":{"kind":"navigate","url":url}}),
                    )
                    .await
                    .map_err(|error| self.why(&id, error))?;
                    id
                }
                None => {
                    // The browser starts on an empty page, so that the journal
                    // is running when the address begins to load.
                    self.browsers.admit(&step.map["open"])?;
                    let params = json!({"selection":{"version":2,
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
                    self.journal(&id).await;
                    let shown = self
                        .inner(
                            &id,
                            "browser_act",
                            json!({"action":{"kind":"navigate","url":url}}),
                        )
                        .await;
                    if let Err(error) = shown {
                        // A session that never showed its first page is not kept.
                        // Why is asked before the session goes.
                        let stop = self.why(&id, error);
                        let _ = self
                            .inner(&id, "browser_control", json!({"action":"close"}))
                            .await;
                        *session = None;
                        return Err(stop);
                    }
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
                        self.brief(&id, done["tab_id"].as_str().unwrap_or(""))
                    ),
                    Value::Null,
                    false,
                ))
            }
            _ => {
                let targets = step.targets()?;
                let mut described = targets
                    .iter()
                    .any(|target| matches!(target, Target::Described { .. }));
                let mut again = false;
                // A date, a time, a colour and a slider take no typing: their
                // value is set, as a list's is.
                let mut as_value = false;
                let until = self.patience();
                let (mut done, written) = loop {
                    let attempt: std::result::Result<_, Stop> = async {
                        let (references, written) =
                            self.resolve(&id, &targets, again, step.verb).await?;
                        let mut params = json!({"action":if as_value {
                            json!({"kind":"select","ref":references[0],"value":step.map["text"]})
                        } else {
                            step.action(&references)
                        }});
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
                        Err(stop)
                            if verb == "fill"
                                && !as_value
                                && stop.code == "browser_not_actionable"
                                && stop.state == "not_sent" =>
                        {
                            as_value = true;
                            again = true;
                            described = true;
                        }
                        Err(stop) if as_value && stop.code == "browser_invalid_params" => {
                            break Err(Stop {
                                code: "browser_value_rejected",
                                state: "not_sent",
                                detail: String::new(),
                            })?;
                        }
                        // A reference that the page has outlived is looked for
                        // once more, by what it stood for.
                        Err(stop)
                            if !again
                                && stop.code == "browser_stale_reference"
                                && stop.state == "not_sent"
                                && targets
                                    .iter()
                                    .any(|target| matches!(target, Target::Reference(_))) =>
                        {
                            again = true;
                            described = true;
                        }
                        // A target given by description is looked up afresh, so a
                        // page that was still loading or redrawing gets another go.
                        // A drag checks where both ends lie once more after the
                        // pointer has gone to the first, and gives up before
                        // anything is dropped when that moved them: leaving what
                        // it hovered over can shift the page. It is begun anew.
                        Err(stop)
                            if described
                                && verb == "drag"
                                && stop.code == "browser_stale_reference"
                                && Instant::now() < until => {}
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
                    let said = match verb {
                        "click"
                            if step.map.contains_key("button") && step.map["button"] == "right" =>
                        {
                            "right-click"
                        }
                        "click" if step.map.get("count") == Some(&json!(2)) => "double-click",
                        _ => verb,
                    };
                    format!("{said} {}", written.join(" to "))
                };
                // A dialog the step raised keeps every later step from running.
                let dialog = self.dialog(&id);
                if let Some(dialog) = &dialog {
                    let _ = write!(
                        line,
                        "\n  {} {} dialog opened: {:?} — answer it with {{\"dialog\":\"accept\"}} or {{\"dialog\":\"dismiss\"}}",
                        if dialog.kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
                            "an"
                        } else {
                            "a"
                        },
                        dialog.kind,
                        dialog.message
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
            meaning: Meaning {
                role: role.into(),
                name: name.into(),
                context: record.into(),
            },
        })
        .collect()
    }

    #[test]
    fn a_reference_is_found_again_only_by_all_it_stood_for() {
        let node = |name: &str, row: &str, value: &str| {
            json!({"role":"button","tag":"button","name":name,
                "record":{"entries":[{"text":row,"value":value},{"text":name,"value":null}]}})
        };
        let meaning = Meaning::of(&node(" Approve ", "Order A2", "typed"));
        // What a field of the row holds is not part of what the row is.
        assert_eq!(meaning, Meaning::of(&node("Approve", "Order A2", "other")));
        assert_eq!(meaning.context, "Order A2 Approve");
        let mut meanings = Meanings::default();
        meanings.keep("tab1", vec![("r1".into(), meaning.clone())]);
        assert_eq!(meanings.of("r1", "tab1"), Some(meaning.clone()));
        assert!(meanings.of("r1", "tab2").is_none() && meanings.of("r2", "tab1").is_none());

        assert_eq!(recall(&meaning, &all()).ok(), Some(1));
        // The row now shows another order: nothing takes the reference's place.
        let moved = Meaning {
            context: "Order A7 Approve".into(),
            ..meaning.clone()
        };
        let gone = recall(&moved, &all()).err().unwrap();
        assert_eq!(gone.code, "browser_stale_reference");
        assert!(
            gone.detail.ends_with("is no longer on the page"),
            "{}",
            gone.detail
        );
        // Two rows that read the same cannot be told apart.
        let mut twice = all();
        twice[0].meaning = meaning.clone();
        let unclear = recall(&meaning, &twice).err().unwrap();
        assert!(
            unclear.detail.ends_with("several elements now are"),
            "{}",
            unclear.detail
        );

        let mut many = Meanings::default();
        for index in 0..Meanings::MOST + 5 {
            many.keep("t", vec![(format!("r{index}"), meaning.clone())]);
        }
        assert!(many.of("r4", "t").is_none() && many.of("r5", "t").is_some());
        assert_eq!(many.known.len(), Meanings::MOST);
    }
    fn described(value: Value) -> Target {
        Target::parse(&value).unwrap()
    }
    #[test]
    fn a_description_stands_for_one_element_or_for_none() {
        let all = all();
        let pick =
            |value| choose(&described(value), &all, &[]).map(|index| all[index].reference.clone());
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
            json!({"click":{"role":"button"}}),
            json!({"click":"r1","button":"right"}),
            json!({"click":"r1","count":2}),
            json!({"click":"r1","button":"left","count":1}),
        ] {
            assert!(Step::parse(&valid).is_ok(), "{valid}");
        }
        for invalid in [
            json!({}),
            json!({"click":"r1","fill":"r2","text":"x"}),
            json!({"click":"r1","text":"x"}),
            json!({"click":{"in":"Draft"}}),
            json!({"click":"r1","button":"middle"}),
            json!({"click":"r1","count":3}),
            json!({"click":"r1","button":"right","count":2}),
            json!({"hover":"r1","count":2}),
            json!({"click":{"name":"","role":"button"}}),
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
