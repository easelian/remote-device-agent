//! A dense projection of one observation, written as lines of text. A fact
//! left out has its ordinary value, a record is written once, above the nodes
//! that belong to it, and nothing is written that another line already says.
use super::sem;
use crate::browser::Result;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

/// A short reference that is never issued twice by this session. The prefix
/// differs between sessions, so a reference carried to another session misses.
pub(super) fn reference(issued: &mut (String, u64)) -> Result<String> {
    if issued.0.is_empty() {
        issued.0 = crate::random::hex_token::<2>().map_err(crate::browser::io_error)?;
    }
    issued.1 += 1;
    Ok(format!("r{}{:x}", issued.0, issued.1))
}

/// Containers that only lay out their content. One with no attribute, value
/// or state of its own is left out of a record unless it is the record itself.
/// The page program budgets by the same list.
const LAYOUT: [&str; 23] = [
    "td", "div", "span", "p", "b", "i", "em", "strong", "small", "u", "button", "tbody", "ul",
    "ol", "sup", "sub", "svg", "g", "path", "use", "circle", "rect", "polygon",
];

/// Attributes by which the page's own code finds an element. They are written
/// only where nothing else tells two controls apart.
const ADDRESS: [&str; 5] = ["id", "name", "for", "tabindex", "aria-labelledby"];

/// A button's type is reported on its own node as `type`.
fn attributes(entry: &sem::Entry, address: bool) -> impl Iterator<Item = &(String, String)> {
    entry.attributes.iter().filter(move |(name, _)| {
        !(name == "type" && entry.tag == "button") && (address || !ADDRESS.contains(&name.as_str()))
    })
}

/// An element the page program takes for a record: a table row, a list item,
/// an article, or an element that states one of those roles.
fn record_element(entry: &sem::Entry) -> bool {
    matches!(entry.tag.as_str(), "tr" | "li" | "article")
        || entry.attributes.iter().any(|(name, value)| {
            name == "role"
                && (value.split_ascii_whitespace().next()).is_some_and(|role| {
                    ["row", "listitem", "article", "option"]
                        .iter()
                        .any(|record| role.eq_ignore_ascii_case(record))
                })
        })
}

/// A node that is read by itself, on one line and under no record: text
/// outside every record, and an element that is its own scope without being a
/// record. What such an element holds is listed by the nodes inside it. A
/// native list keeps its options on a record line.
fn flat(facts: &sem::Facts) -> bool {
    let entries = &facts.record.entries;
    let Some(first) = entries.first() else {
        return true;
    };
    if is_text(facts) {
        // The block the text is set in, its own parent where that differs, and the text.
        return entries.len() == 1
            || entries.len() <= 3
                && entries
                    .last()
                    .is_some_and(|last| last.identity == facts.node)
                && entries[..entries.len() - 1]
                    .iter()
                    .all(|entry| entry.tag != "#text")
                && !record_element(first);
    }
    own(facts) && !record_element(first) && facts.tag != "select"
}

/// The scheme and host of a document, by which its own links are shortened.
fn origin(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    (host > 0).then(|| &url[..url.len() - rest.len() + host])
}

/// A link into the document's own site is written from its path on.
fn local<'a>(name: &str, value: &'a str, site: Option<&str>) -> &'a str {
    if !matches!(name, "href" | "action" | "formaction") {
        return value;
    }
    match site.and_then(|site| value.strip_prefix(site)) {
        Some("") => "/",
        Some(rest) if rest.starts_with(['/', '?', '#']) => rest,
        _ => value,
    }
}

/// The record starts at the node's own element.
fn own(facts: &sem::Facts) -> bool {
    let entries = &facts.record.entries;
    entries.len() == 1
        || entries
            .first()
            .is_some_and(|entry| entry.identity == facts.node)
}

/// The role a container states for itself where this profile models none.
fn stated(facts: &sem::Facts) -> Option<&str> {
    if facts.role != "unsupported" || !own(facts) {
        return None;
    }
    let (_, role) =
        (facts.record.entries.first()?.attributes.iter()).find(|(name, _)| name == "role")?;
    let role = role.split_whitespace().next()?;
    role.bytes()
        .all(|byte| byte.is_ascii_lowercase())
        .then_some(role)
}

/// An attribute of the node's own element that the node's line already states.
fn restated(facts: &sem::Facts, name: &str, value: &str) -> bool {
    match name {
        "type" => value == facts.input_type,
        "aria-label" => value.trim() == facts.name.trim(),
        "role" => value == facts.role || stated(facts) == Some(value),
        "readonly" => facts.readonly,
        "aria-readonly" => value == "true" && facts.readonly,
        "disabled" => !facts.enabled,
        "aria-disabled" => value == "true" && !facts.enabled,
        "aria-checked" => value == "true" && facts.checked,
        "contenteditable" => facts.editable,
        _ => false,
    }
}

fn squeezed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The material says nothing beyond the node's own name, value and state.
fn plain(facts: &sem::Facts, address: bool, listed: bool) -> bool {
    let record = &facts.record;
    let own = listed && own(facts);
    record.complete
        && record.limits.is_empty()
        && record.entries.iter().enumerate().all(|(index, entry)| {
            attributes(entry, address).all(|(name, value)| {
                name == "type" && *value == facts.input_type
                    || own && index == 0 && restated(facts, name, value)
            }) && (entry.value.as_deref().is_none_or(str::is_empty) || entry.value == facts.value)
                && (!entry.checked || facts.checked)
                && if entry.tag == "#text" {
                    entry.text.trim() == facts.name.trim()
                } else {
                    entry.text.is_empty()
                }
        })
}

/// A container whose role this profile does not model, with no name and no
/// reference, tells the reader nothing that its record does not.
fn silent(facts: &sem::Facts, full: &Value) -> bool {
    facts.role == "unsupported" && facts.name.is_empty() && full.get("ref").is_none()
}

/// A container with no name and no reference whose role only groups or lays out.
fn idle(facts: &sem::Facts, full: &Value) -> bool {
    silent(facts, full)
        && stated(facts).is_none_or(|role| {
            matches!(
                role,
                "presentation" | "none" | "generic" | "list" | "group" | "rowgroup"
            )
        })
        && (facts.record.entries.first()).is_none_or(|entry| {
            attributes(entry, false).all(|(name, value)| restated(facts, name, value))
        })
}

fn is_text(facts: &sem::Facts) -> bool {
    facts.kind == "text" && facts.role == "text"
}

/// Limits that hold for every page under this profile; the contract states them once.
const STANDING: [&str; 3] = [
    "closed_shadow_not_observable",
    "canvas_pixels_outside_profile",
    "browser_ui_outside_profile",
];

/// Leave out of a compact observation what holds for every reply: the standing
/// limits of the profile, a read that did not change, and a page that is not scrolled.
pub(super) fn trim(value: &mut Value) {
    if let Some(coverage) = value["coverage"].as_object_mut() {
        if let Some(omitted) = coverage.get_mut("omitted").and_then(Value::as_array_mut) {
            omitted.retain(|item| !item.as_str().is_some_and(|item| STANDING.contains(&item)));
            if omitted.is_empty() {
                coverage.remove("omitted");
            }
        }
        if coverage.get("changed_during_read") == Some(&Value::Bool(false)) {
            coverage.remove("changed_during_read");
        }
    }
    if let Some(page) = value["page"].as_object_mut() {
        for axis in ["scroll_x", "scroll_y"] {
            if page.get(axis).and_then(Value::as_f64) == Some(0.0) {
                page.remove(axis);
            }
        }
    }
}

/// A value after `=`. Anything that could be taken for the end of the value is quoted.
fn word(value: &str) -> String {
    let bare = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || "_-.:/@#%+,~".contains(c));
    if bare {
        value.into()
    } else {
        Value::from(value).to_string()
    }
}

/// Page text on one line. Within a record `<` opens an element and `|` parts two texts.
fn inline(text: &str, record: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' | '|' if record => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

enum Item {
    Text(String),
    Element(String),
}

/// The line of a record: its own element, then the text and the elements with
/// facts of their own that it holds, in page order.
struct Record {
    items: Vec<Item>,
}

impl Record {
    /// `None` when the record is the node's own element and adds nothing to its
    /// line. `address` writes the attributes the page's code finds elements by.
    /// `elsewhere` tells whether a text is the name of a node outside the record.
    /// A node that is not `listed` has no line that states anything.
    fn of(
        facts: &sem::Facts,
        address: bool,
        listed: bool,
        site: Option<&str>,
        elsewhere: &dyn Fn(&str) -> bool,
    ) -> Option<Self> {
        if flat(facts) {
            return None;
        }
        let material = &facts.record;
        // What the line of the node already states is not stated again for
        // the node's own element.
        let own = listed && own(facts);
        let whole = material.complete && material.limits.is_empty();
        // What an element holds besides the options of a list is listed by the
        // nodes inside it, or is the element's own name.
        let cut = (facts.name_quality.iter()).any(|limit| limit != "unsupported_role");
        let apart = own && whole && !cut && facts.tag != "select";
        let named = apart
            && facts.name_source == "content"
            && squeezed(&facts.name)
                == squeezed(
                    &(material.entries.iter())
                        .filter(|entry| entry.tag == "#text")
                        .map(|entry| entry.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
        let mut items = Vec::new();
        let mut said = false;
        // A record cut short at a limit is only its own element: every node
        // inside it has a line of its own.
        let listed_entries = if whole { material.entries.len() } else { 1 };
        for (index, entry) in material.entries.iter().take(listed_entries).enumerate() {
            let root = own && index == 0;
            let value = entry.value.as_deref().filter(|value| !value.is_empty());
            if entry.tag == "#text"
                && entry.attributes.is_empty()
                && value.is_none()
                && !entry.checked
            {
                let text = entry.text.trim();
                if !text.is_empty() && !(index > 0 && (named || apart && elsewhere(text))) {
                    items.push(Item::Text(text.into()));
                }
                continue;
            }
            let mut element = format!("<{}", entry.tag);
            let bare = element.len();
            for (name, value) in attributes(entry, address) {
                if root && restated(facts, name, value) {
                    continue;
                }
                if value.is_empty() {
                    let _ = write!(element, " {name}");
                } else {
                    let _ = write!(element, " {name}={}", word(local(name, value, site)));
                }
            }
            if let Some(value) = value.filter(|_| !(root && entry.value == facts.value)) {
                let _ = write!(element, " value={}", word(value));
            }
            if entry.checked && !(root && facts.checked) {
                element.push_str(" checked");
            }
            if index > 0
                && element.len() == bare
                && entry.text.is_empty()
                && LAYOUT.contains(&entry.tag.as_str())
            {
                continue;
            }
            if index == 0 {
                said = element.len() > bare || !entry.text.is_empty();
            }
            element.push('>');
            items.push(Item::Element(element));
            if !entry.text.trim().is_empty() {
                items.push(Item::Text(entry.text.trim().into()));
            }
        }
        if own && whole && !said && items.len() == 1 && matches!(items[0], Item::Element(_)) {
            return None;
        }
        Some(Self { items })
    }

    /// `names` are the names of the controls listed under this line. A text of
    /// the record that only repeats one of them is left to that control's line.
    fn line(&self, mut names: Vec<&str>) -> String {
        let mut out = String::new();
        let mut after_text = false;
        // Where the latest element starts, while no text has followed it.
        let mut element_at = None;
        for (index, item) in self.items.iter().enumerate() {
            match item {
                Item::Text(text) => {
                    if let Some(at) = names.iter().position(|name| name.trim() == text) {
                        names.swap_remove(at);
                        // The element before it is the control's own. With no
                        // fact of its own it goes too; otherwise the next text
                        // is parted from it, so it is not read as its content.
                        match element_at.take() {
                            Some(start) if !out[start..].contains(' ') => {
                                out.truncate(start.saturating_sub(1));
                            }
                            Some(_) => after_text = true,
                            None => {}
                        }
                        continue;
                    }
                    element_at = None;
                    if out.is_empty() {
                        out.push_str("<>");
                    }
                    out.push_str(if after_text { " | " } else { " " });
                    out.push_str(&inline(text, true));
                    after_text = true;
                }
                Item::Element(element) => {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    element_at = (index > 0).then_some(out.len());
                    out.push_str(element);
                    after_text = false;
                }
            }
        }
        if out.is_empty() {
            out.push_str("<>");
        }
        out
    }

    /// The line of a record that was written above and goes on after other content.
    fn resumed(&self) -> String {
        match self.items.first() {
            Some(Item::Element(element)) => format!("{element} …"),
            _ => "<> …".into(),
        }
    }
}

/// `address` writes the attributes the page's code finds the element by, and
/// `site` is the origin of the node's document.
fn node(facts: &sem::Facts, full: &Value, address: bool, site: Option<&str>) -> String {
    let mut out = String::from(if is_text(facts) {
        "text"
    } else if facts.role.is_empty() {
        "element"
    } else {
        stated(facts).unwrap_or(facts.role.as_str())
    });
    if let Some(reference) = full.get("ref").and_then(Value::as_str) {
        let _ = write!(out, " [{reference}]");
        if !facts.in_view {
            out.push_str(" offscreen");
        }
    }
    if !matches!(facts.kind.as_str(), "native" | "text") && facts.kind != facts.role {
        let _ = write!(out, " kind={}", word(&facts.kind));
    }
    let checkable = matches!(
        facts.role.as_str(),
        "radio" | "checkbox" | "switch" | "menuitemcheckbox" | "menuitemradio"
    );
    // An empty value is reported only where one can be typed, and `on` is the
    // value of every box that names none.
    if let Some(value) = (facts.value.as_deref())
        .filter(|value| !value.is_empty() || facts.editable)
        .filter(|value| !(checkable && *value == "on"))
    {
        let _ = write!(out, " value={}", word(value));
    }
    for (name, set) in [
        ("disabled", !facts.enabled),
        ("readonly", facts.readonly),
        // A native text field is one that can be typed in.
        (
            "editable",
            facts.editable
                && !(facts.kind == "native"
                    && matches!(facts.role.as_str(), "textbox" | "searchbox" | "spinbutton")),
        ),
        ("sensitive", facts.sensitive),
    ] {
        if set {
            let _ = write!(out, " {name}");
        }
    }
    if facts.checked {
        out.push_str(" checked");
    } else if checkable {
        out.push_str(" unchecked");
    }
    // The role written in place of `unsupported` says the same as this limit.
    let quality: Vec<&str> = (facts.name_quality.iter().map(String::as_str))
        .filter(|limit| !(*limit == "unsupported_role" && stated(facts).is_some()))
        .collect();
    if !quality.is_empty() {
        let _ = write!(out, " quality={}", word(&quality.join(",")));
    }
    // A button that submits is the ordinary kind of button, and these roles
    // have one input type each.
    let ordinary = matches!(facts.input_type.as_str(), "" | "button" | "text")
        || matches!(
            (facts.role.as_str(), facts.input_type.as_str()),
            ("button", "submit")
                | ("checkbox", "checkbox")
                | ("radio", "radio")
                | ("combobox", "select-one")
                | ("listbox", "select-multiple")
                | ("slider", "range")
                | ("spinbutton", "number")
                | ("searchbox", "search")
        );
    if !ordinary {
        let _ = write!(out, " type={}", word(&facts.input_type));
    }
    if facts.role.is_empty() || facts.kind == "inferred" {
        let _ = write!(out, " tag={}", word(&facts.tag));
    }
    // An element read by itself states its own attributes here. A tooltip
    // beside a name is left to the full projection.
    if !is_text(facts)
        && flat(facts)
        && let Some(entry) = facts.record.entries.first()
    {
        for (name, value) in attributes(entry, address) {
            if restated(facts, name, value) || name == "title" && !facts.name.is_empty() {
                continue;
            }
            if value.is_empty() {
                let _ = write!(out, " {name}");
            } else {
                let _ = write!(out, " {name}={}", word(local(name, value, site)));
            }
        }
    }
    if !facts.name.is_empty() {
        out.push_str(": ");
        out.push_str(&inline(&facts.name, false));
    }
    out
}

/// Replace the per-node projection in `value` by lines of text. `full` holds
/// the ordinary projection of the same rows, from which frame IDs and
/// references are taken.
///
/// A node with a record is indented under the line of that record. Text that
/// the record line already lists is not written again as a node.
pub(super) fn write(value: &mut Value, rows: &[sem::Row], full: &[Value], frames: &[sem::Frame]) {
    let root = |position: usize| {
        let entries = &rows[position].facts.record.entries;
        entries.first().map(|entry| entry.identity)
    };
    // The records that hold a node of each name, two at most.
    let mut holders: HashMap<(sem::Context, &str), Vec<Option<u64>>> = HashMap::new();
    for (position, row) in rows.iter().enumerate() {
        let held = holders
            .entry((row.context, row.facts.name.trim()))
            .or_default();
        if held.len() < 2 && !held.contains(&root(position)) {
            held.push(root(position));
        }
    }
    let site = |position: usize| {
        (frames.iter())
            .find(|frame| frame.context == rows[position].context)
            .and_then(|frame| origin(&frame.url))
    };
    let record = |position: usize, address: bool| {
        let row = &rows[position];
        Record::of(
            &row.facts,
            address,
            !silent(&row.facts, &full[position]),
            site(position),
            &|text| {
                (holders.get(&(row.context, text)).into_iter().flatten())
                    .any(|held| *held != root(position))
            },
        )
    };
    let mut records: Vec<Option<Record>> = (0..rows.len()).map(|at| record(at, false)).collect();
    // Controls that read alike are told apart by how the page addresses them.
    let alike = |position: usize, records: &[Option<Record>]| {
        let row = &rows[position];
        // A node read by itself is told apart by its own line.
        let line = if flat(&row.facts) {
            Some(node(&row.facts, &Value::Null, false, site(position)))
        } else {
            records[position]
                .as_ref()
                .map(|record| record.line(Vec::new()))
        };
        (
            row.context,
            row.facts.role.as_str(),
            row.facts.name.as_str(),
            line,
        )
    };
    let mut twins: HashMap<_, u32> = HashMap::new();
    for position in (0..rows.len()).filter(|&at| full[at].get("ref").is_some()) {
        *twins.entry(alike(position, &records)).or_default() += 1;
    }
    let address: Vec<bool> = (0..rows.len())
        .map(|at| full[at].get("ref").is_some() && twins[&alike(at, &records)] > 1)
        .collect();
    drop(twins);
    // A run of nodes that share a record is headed by its line, unless the
    // record says nothing beyond what each of those nodes says itself.
    let mut start = 0;
    while start < rows.len() {
        let end = (start + 1..rows.len())
            .find(|&next| {
                rows[next].context != rows[start].context
                    || root(start).is_none()
                    || root(next) != root(start)
            })
            .unwrap_or(rows.len());
        let address = address[start..end].contains(&true);
        if address {
            (start..end).for_each(|at| records[at] = record(at, true));
        }
        if (start..end).all(|at| {
            flat(&rows[at].facts)
                || plain(
                    &rows[at].facts,
                    address,
                    !silent(&rows[at].facts, &full[at]),
                )
        }) {
            records[start..end]
                .iter_mut()
                .for_each(|record| *record = None);
        }
        start = end;
    }
    // The text of a label is the name of the control it labels.
    let labelled: HashSet<(sem::Context, &str)> = rows
        .iter()
        .filter(|row| !is_text(&row.facts) && row.facts.name_source == "label")
        .map(|row| (row.context, row.facts.name.trim()))
        .collect();
    let label: Vec<bool> = (rows.iter().zip(full))
        .map(|(row, full)| {
            let facts = &row.facts;
            is_text(facts)
                && full.get("ref").is_none()
                && facts.record.complete
                && facts.name_quality.is_empty()
                && (facts.record.entries.iter())
                    .take(if flat(facts) { 2 } else { 1 })
                    .any(|entry| entry.tag == "label")
                && labelled.contains(&(row.context, facts.name.trim()))
        })
        .collect();
    (0..rows.len())
        .filter(|&at| label[at])
        .for_each(|at| records[at] = None);
    let same = |a: usize, b: usize| {
        rows[a].context == rows[b].context
            && records[a].is_some()
            && records[b].is_some()
            && root(a) == root(b)
    };
    // Text the record lists, read whole, with nothing to act on.
    let repeated = |position: usize| {
        let facts = &rows[position].facts;
        records[position].is_some()
            && is_text(facts)
            && full[position].get("ref").is_none()
            && facts.record.complete
            && facts.name_quality.is_empty()
            && facts
                .record
                .entries
                .iter()
                .any(|entry| entry.tag == "#text" && entry.text.trim() == facts.name.trim())
    };
    let mut lines: Vec<String> = Vec::new();
    // The child frame whose heading the latest line stands under.
    let mut section: Option<sem::Context> = None;
    let mut head = |lines: &mut Vec<String>, position: usize| {
        let context = rows[position].context;
        let child = frames
            .iter()
            .any(|frame| frame.context == context && frame.parent.is_some());
        if child && section != Some(context) {
            let id = full[position]["frame_id"].as_str().unwrap_or_default();
            lines.push(format!("--- iframe {id}"));
            section = Some(context);
        } else if !child && section.is_some() {
            lines.push("--- page".into());
            section = None;
        }
    };
    // Pieces of text set in one block, with nothing listed between them, read as one line.
    let piece = |position: usize| {
        let facts = &rows[position].facts;
        let entries = &facts.record.entries;
        (is_text(facts)
            && flat(facts)
            && entries.len() > 1
            && !label[position]
            && facts.name_quality.is_empty()
            && full[position].get("ref").is_none())
        .then(|| (rows[position].context, entries[0].identity))
    };
    let mut joined: Option<(sem::Context, u64)> = None;
    // The records whose line is written already.
    let mut written: HashSet<(sem::Context, u64)> = HashSet::new();
    for position in 0..rows.len() {
        let facts = &rows[position].facts;
        if !label[position] {
            let block = piece(position);
            if block.is_some()
                && block == joined
                && let Some(last) = lines.last_mut()
            {
                last.push(' ');
                last.push_str(&inline(&facts.name, false));
                continue;
            }
            joined = block;
        }
        if let Some(record) = &records[position]
            && (position == 0 || !same(position - 1, position))
        {
            let names = (position..rows.len())
                .take_while(|&next| same(position, next))
                .filter(|&next| {
                    !is_text(&rows[next].facts) && !silent(&rows[next].facts, &full[next])
                })
                .map(|next| rows[next].facts.name.as_str())
                .collect();
            head(&mut lines, position);
            let first =
                root(position).is_none_or(|root| written.insert((rows[position].context, root)));
            lines.push(if first {
                record.line(names)
            } else {
                record.resumed()
            });
        }
        let quiet = if flat(facts) {
            idle(facts, &full[position])
        } else {
            silent(facts, &full[position])
        };
        if label[position] || repeated(position) || quiet {
            continue;
        }
        head(&mut lines, position);
        let indent = if records[position].is_some() {
            "  "
        } else {
            ""
        };
        lines.push(format!(
            "{indent}{}",
            node(facts, &full[position], address[position], site(position))
        ));
    }
    value["format"] = "compact_v1".into();
    value["text"] = lines.join("\n").into();
}
