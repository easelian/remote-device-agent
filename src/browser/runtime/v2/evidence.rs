//! Session-owned, expiring public diagnostic identities. No page content or
//! native object is retained; publication uses the runtime's control fence.
use super::*;
use sem::evidence::{self as dom, Query, Raw};
use sha2::{Digest, Sha256};

const MAX_ENTRIES: usize = 128;
const MAX_ALLOCATION: usize = 32 * 1024;
const TTL: Duration = Duration::from_secs(60);
#[derive(Clone, Copy)]
struct Entry {
    id: [u8; 36],
    tab: [u8; 32],
    target: dom::Target,
    epoch: u64,
    created: Instant,
}
const _: () = assert!(std::mem::size_of::<Entry>() * MAX_ENTRIES <= MAX_ALLOCATION);
#[derive(Default)]
pub(super) struct Registry {
    entries: Vec<Entry>,
}
impl Registry {
    pub fn clear(&mut self) {
        self.entries.clear();
    }
    pub fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|e| now.saturating_duration_since(e.created) < TTL);
    }
    pub fn lookup(&mut self, id: &str, tab: &str, epoch: u64) -> Result<dom::Target> {
        self.expire(Instant::now());
        let tab: [u8; 32] = Sha256::digest(tab.as_bytes()).into();
        self.entries
            .iter()
            .find(|e| e.id.as_slice() == id.as_bytes() && e.tab == tab && e.epoch == epoch)
            .map(|e| e.target)
            .ok_or_else(sem::types::stale)
    }
    pub fn publish(&mut self, prepared: Prepared) {
        self.expire(Instant::now());
        if self.entries.capacity() == 0 {
            self.entries = Vec::with_capacity(MAX_ENTRIES);
        }
        for entry in prepared.entries {
            if self.entries.len() == MAX_ENTRIES {
                self.entries.remove(0);
            }
            self.entries.push(entry);
        }
        debug_assert!(self.entries.capacity() * std::mem::size_of::<Entry>() <= MAX_ALLOCATION);
    }
}
pub(super) struct Prepared {
    entries: Vec<Entry>,
}
fn entry(target: dom::Target, tab: &str, epoch: u64) -> Result<(String, Entry)> {
    let id = format!("den_{}", token()?);
    let encoded: [u8; 36] = id.as_bytes().try_into().map_err(|_| sem::types::decode())?;
    Ok((
        id,
        Entry {
            id: encoded,
            tab: Sha256::digest(tab.as_bytes()).into(),
            target,
            epoch,
            created: Instant::now(),
        },
    ))
}
fn public_node(node: dom::Node, id: &str, index: usize, main: bool) -> Result<Value> {
    let mut value = serde_json::to_value(node).map_err(|_| sem::types::decode())?;
    value.as_object_mut().unwrap().remove("node");
    value["node_id"] = id.into();
    value["frame_index"] = index.into();
    value["main_frame"] = main.into();
    Ok(value)
}
pub(super) fn prepare(
    capture: dom::Capture,
    query: &Query,
    tab: &str,
    epoch: u64,
) -> Result<(Value, Prepared)> {
    let mut value = json!({"version":1,"kind":query.kind(),"tab_id":tab,"source":query.source(),"content_trust":"untrusted_page",
        "captured":{"started_at_ms":capture.started_at_ms,"finished_at_ms":capture.finished_at_ms},"coverage":capture.coverage});
    value["coverage"]["scope"] = query.scope().into();
    value["coverage"]["frames_read"] = capture.records.len().into();
    let mut entries = Vec::new();
    let mut nodes = Vec::new();
    for (index, raw) in capture.records {
        let frame = &capture.frames[index];
        let main = frame.parent.is_none();
        match raw {
            Raw::FindNodes { nodes: rows, .. } => {
                for node in rows {
                    let (id, binding) =
                        entry(dom::target(&capture.frames, frame, node.node)?, tab, epoch)?;
                    nodes.push(public_node(node, &id, index, main)?);
                    entries.push(binding);
                }
            }
            Raw::ReadNode { node, .. } => {
                let mut facts = public_node(
                    node.facts,
                    query.id().ok_or_else(sem::types::stale)?,
                    index,
                    main,
                )?;
                facts["text_excerpt"] = node.text_excerpt.into();
                facts["states"] =
                    serde_json::to_value(node.states).map_err(|_| sem::types::decode())?;
                facts["attributes"] =
                    serde_json::to_value(node.attributes).map_err(|_| sem::types::decode())?;
                value["node"] = facts;
            }
            Raw::ReadAccessibility { accessibility, .. } => {
                let mut facts =
                    serde_json::to_value(accessibility).map_err(|_| sem::types::decode())?;
                facts["node_id"] = query.id().ok_or_else(sem::types::stale)?.into();
                facts["frame_index"] = index.into();
                facts["main_frame"] = main.into();
                value["accessibility"] = facts;
            }
            Raw::ReadLayout { layout, .. } => {
                let hit = layout.hit.node;
                let mut layout = serde_json::to_value(layout).map_err(|_| sem::types::decode())?;
                layout["node_id"] = query.id().ok_or_else(sem::types::stale)?.into();
                layout["frame_index"] = index.into();
                layout["main_frame"] = main.into();
                layout["coordinate_space"] = "frame_viewport".into();
                layout["hit"].as_object_mut().unwrap().remove("node");
                if let Some(node) = hit {
                    let (id, binding) =
                        entry(dom::target(&capture.frames, frame, node)?, tab, epoch)?;
                    layout["hit"]["node_id"] = id.into();
                    entries.push(binding);
                }
                value["layout"] = layout;
            }
        }
    }
    if matches!(query, Query::FindNodes { .. }) {
        value["nodes"] = nodes.into();
    }
    Ok((value, Prepared { entries }))
}
pub(super) fn bounded(value: &mut Value, prepared: &mut Prepared) -> Result<()> {
    loop {
        if serde_json::to_vec(value)
            .map_err(|_| sem::types::decode())?
            .len()
            <= dom::MAX_BYTES
        {
            return Ok(());
        }
        let Some(nodes) = value
            .get_mut("nodes")
            .and_then(Value::as_array_mut)
            .filter(|nodes| !nodes.is_empty())
        else {
            return Err(Failure::new("result_too_large"));
        };
        nodes.pop();
        prepared.entries.pop();
        value["coverage"]["truncated"] = true.into();
        let limits = value["coverage"]["limits"].as_array_mut().unwrap();
        if !limits.iter().any(|v| v == "byte_limit") {
            limits.push("byte_limit".into());
        }
    }
}
#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
