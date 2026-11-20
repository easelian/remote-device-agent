//! A dense projection of one observation. A field left out has its ordinary
//! value, and a record shared by several nodes is written once.
use super::sem;
use crate::browser::Result;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

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
const LAYOUT: [&str; 12] = [
    "td", "div", "span", "p", "b", "i", "em", "strong", "small", "u", "button", "tbody",
];

/// A button's type is reported on its own node as input_type.
fn attributes(entry: &sem::Entry) -> impl Iterator<Item = &(String, String)> {
    entry
        .attributes
        .iter()
        .filter(move |(name, _)| !(name == "type" && entry.tag == "button"))
}

fn entry(entry: &sem::Entry) -> Value {
    let mut detail = Map::new();
    let attributes: Map<_, _> = attributes(entry)
        .map(|(name, value)| (name.clone(), Value::from(value.as_str())))
        .collect();
    if !attributes.is_empty() {
        detail.insert("attributes".into(), attributes.into());
    }
    if let Some(value) = entry.value.as_deref().filter(|value| !value.is_empty()) {
        detail.insert("value".into(), value.into());
    }
    if entry.checked {
        detail.insert("checked".into(), true.into());
    }
    if entry.tag == "#text" && detail.is_empty() {
        return entry.text.as_str().into();
    }
    if !entry.text.is_empty() {
        detail.insert("text".into(), entry.text.as_str().into());
    }
    if detail.is_empty() {
        json!([entry.tag])
    } else {
        json!([entry.tag, detail])
    }
}

/// The material says nothing beyond the node's own name, value and state.
fn plain(facts: &sem::Facts) -> bool {
    let record = &facts.record;
    record.complete
        && record.limits.is_empty()
        && record.entries.iter().all(|entry| {
            attributes(entry).all(|(name, value)| name == "type" && *value == facts.input_type)
                && (entry.value.as_deref().is_none_or(str::is_empty) || entry.value == facts.value)
                && (!entry.checked || facts.checked)
                && if entry.tag == "#text" {
                    entry.text.trim() == facts.name.trim()
                } else {
                    entry.text.is_empty()
                }
        })
}

fn node(facts: &sem::Facts, full: &Value, record: Option<usize>) -> Value {
    let mut node = Map::new();
    // A node with no role is text of the page.
    if facts.kind != "text" || facts.role != "text" {
        node.insert("role".into(), facts.role.as_str().into());
    }
    node.insert("name".into(), facts.name.as_str().into());
    if let Some(reference) = full.get("ref") {
        node.insert("ref".into(), reference.clone());
        if !facts.in_view {
            node.insert("offscreen".into(), true.into());
        }
    }
    if let Some(record) = record {
        node.insert("record".into(), record.into());
    }
    if !matches!(facts.kind.as_str(), "native" | "text") {
        node.insert("kind".into(), facts.kind.as_str().into());
    }
    // An empty value is reported only where one can be typed.
    if let Some(value) = facts
        .value
        .as_deref()
        .filter(|value| !value.is_empty() || facts.editable)
    {
        node.insert("value".into(), value.into());
    }
    if !facts.enabled {
        node.insert("disabled".into(), true.into());
    }
    for (name, set) in [
        ("readonly", facts.readonly),
        ("editable", facts.editable),
        ("sensitive", facts.sensitive),
    ] {
        if set {
            node.insert(name.into(), true.into());
        }
    }
    if facts.checked
        || matches!(
            facts.role.as_str(),
            "radio" | "checkbox" | "switch" | "menuitemcheckbox" | "menuitemradio"
        )
    {
        node.insert("checked".into(), facts.checked.into());
    }
    if !facts.name_quality.is_empty() {
        node.insert("name_quality".into(), json!(facts.name_quality));
    }
    if facts.name_source != "content" && !facts.name.is_empty() {
        node.insert("name_source".into(), facts.name_source.as_str().into());
    }
    if !matches!(facts.input_type.as_str(), "" | "button" | "text") {
        node.insert("input_type".into(), facts.input_type.as_str().into());
    }
    if facts.role.is_empty() || facts.kind == "inferred" {
        node.insert("tag".into(), facts.tag.as_str().into());
    }
    node.into()
}

/// Replace the per-node projection in `value`. `full` holds the ordinary
/// projection of the same rows, from which frame IDs and references are taken.
///
/// Text that a written record already lists is not repeated as a node of its
/// own when a control of that record is returned. With no such control the
/// first text of the record stays, so the record keeps its place in the page.
pub(super) fn write(value: &mut Value, rows: &[sem::Row], full: &[Value], frames: &[sem::Frame]) {
    let mut records = Vec::new();
    let mut known: HashMap<(sem::Context, String), usize> = HashMap::new();
    // For each record: whether a returned node other than text belongs to it,
    // and the first row that does.
    let mut members: Vec<(bool, usize)> = Vec::new();
    let is_text = |facts: &sem::Facts| facts.kind == "text" && facts.role == "text";
    let assigned: Vec<Option<usize>> = rows
        .iter()
        .enumerate()
        .map(|(position, row)| {
            let facts = &row.facts;
            if plain(facts) {
                return None;
            }
            let mut body = Map::new();
            let entries: Vec<_> = facts
                .record
                .entries
                .iter()
                .enumerate()
                .map(|(index, item)| (index, entry(item)))
                .filter(|(index, value)| {
                    *index == 0
                        || value.as_array().is_none_or(|parts| {
                            parts.len() > 1 || !LAYOUT.contains(&parts[0].as_str().unwrap_or(""))
                        })
                })
                .map(|(_, value)| value)
                .collect();
            body.insert("entries".into(), entries.into());
            if !facts.record.complete {
                body.insert("complete".into(), false.into());
            }
            if !facts.record.limits.is_empty() {
                body.insert("limits".into(), json!(facts.record.limits));
            }
            let body = Value::Object(body);
            let index = *known
                .entry((row.context, body.to_string()))
                .or_insert_with(|| {
                    records.push(body);
                    members.push((false, position));
                    records.len() - 1
                });
            members[index].0 |= !is_text(facts);
            Some(index)
        })
        .collect();
    let mut groups: Vec<Value> = Vec::new();
    let mut open: Option<(sem::Context, Value, Vec<Value>)> = None;
    let mut close = |open: &mut Option<(sem::Context, Value, Vec<Value>)>| {
        if let Some((context, id, nodes)) = open.take() {
            let child = frames
                .iter()
                .any(|frame| frame.context == context && frame.parent.is_some());
            groups.push(
                json!({"frame_id":id,"scope":if child {"iframe"} else {"page"},"nodes":nodes}),
            );
        }
    };
    for (position, ((row, full), record)) in rows.iter().zip(full).zip(&assigned).enumerate() {
        let facts = &row.facts;
        if open
            .as_ref()
            .is_none_or(|(context, ..)| *context != row.context)
        {
            close(&mut open);
            open = Some((row.context, full["frame_id"].clone(), Vec::new()));
        }
        if let Some(index) = record
            && is_text(facts)
            && full.get("ref").is_none()
            && facts.record.complete
            && facts.name_quality.is_empty()
            && (members[*index].0 || members[*index].1 != position)
            && facts
                .record
                .entries
                .iter()
                .any(|entry| entry.tag == "#text" && entry.text.trim() == facts.name.trim())
        {
            continue;
        }
        open.as_mut().unwrap().2.push(node(facts, full, *record));
    }
    close(&mut open);
    groups.retain(|group| {
        group["nodes"]
            .as_array()
            .is_some_and(|nodes| !nodes.is_empty())
    });
    value["format"] = "compact_v1".into();
    value["records"] = records.into();
    value["frames"] = groups.into();
}
