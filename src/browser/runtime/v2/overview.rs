//! A compact view of the rows already returned by this observation.
use super::sem;
use serde_json::{Value, json};
use std::collections::HashMap;

const ITEMS_PER_FRAME: usize = 16;
const TOTAL_ITEMS: usize = 40;
const LABEL_CHARS: usize = 160;

pub(super) fn build(
    rows: &[sem::Row],
    nodes: &[Value],
    frames: &[sem::Frame],
    incomplete: bool,
    max_chars: usize,
) -> Value {
    // A material entry identifies a DOM node privately. Disclose a record label
    // only if the sole heading in that material also appeared as a visible row.
    let headings: HashMap<_, _> = rows
        .iter()
        .filter_map(|row| {
            let f = &row.facts;
            let root = f.record.entries.first()?.identity;
            (f.role == "heading"
                && f.visible
                && !f.sensitive
                && f.record.complete
                && !f.name.is_empty()
                && f.name.chars().count() <= LABEL_CHARS)
                .then_some(((row.context, root, f.node), f.name.as_str()))
        })
        .collect();
    let mut groups = Vec::new();
    let mut truncated = incomplete;
    let mut count = 0;
    // Count encoded bytes (a conservative bound for non-ASCII text), including
    // frame headers and punctuation. This also caps the size of a many-frame view.
    let mut remaining = max_chars.saturating_sub(r#"{"frames":[],"truncated":false}"#.len());
    let ordered = frames
        .iter()
        .filter(|f| f.parent.is_some())
        .chain(frames.iter().filter(|f| f.parent.is_none()));
    for frame in ordered {
        let mut items = Vec::new();
        let mut group = Value::Null;
        for (row, node) in rows
            .iter()
            .zip(nodes)
            .filter(|(row, _)| row.context == frame.context)
        {
            let f = &row.facts;
            if !f.visible || f.sensitive {
                continue;
            }
            if count == TOTAL_ITEMS || items.len() == ITEMS_PER_FRAME {
                truncated = true;
                break;
            }
            if group.is_null() {
                let scope = if frame.parent.is_some() {
                    "iframe"
                } else {
                    "page"
                };
                group = json!({"frame_id":node["frame_id"],"scope":scope,"items":[]});
                let overhead = group.to_string().len() + 1;
                if overhead > remaining {
                    truncated = true;
                    break;
                }
                remaining -= overhead;
            }
            let mut item =
                json!({"role":f.role,"name":f.name.chars().take(LABEL_CHARS).collect::<String>()});
            if f.name.chars().nth(LABEL_CHARS).is_some() {
                item["name_truncated"] = true.into();
                truncated = true;
            }
            if let Some(reference) = node.get("ref") {
                item["ref"] = reference.clone();
            }
            if matches!(f.role.as_str(), "radio" | "checkbox" | "switch") {
                item["checked"] = f.checked.into();
            }
            if f.record.complete
                && let Some(root) = f.record.entries.first()
            {
                let mut entries = f.record.entries.iter().filter(|e| {
                    matches!(e.tag.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
                        || e.attributes
                            .iter()
                            .any(|(k, v)| k == "role" && v == "heading")
                });
                if let Some(heading) = entries.next()
                    && entries.next().is_none()
                    && let Some(label) =
                        headings.get(&(row.context, root.identity, heading.identity))
                {
                    item["record"] = (*label).into();
                }
            }
            let size = item.to_string().len() + 1;
            if size > remaining {
                truncated = true;
                continue;
            }
            remaining -= size;
            items.push(item);
            count += 1;
        }
        if !items.is_empty() {
            group["items"] = items.into();
            groups.push(group);
        }
    }
    json!({"frames":groups,"truncated":truncated})
}
