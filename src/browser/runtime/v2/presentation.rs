//! Optional display baselines. Native identity and input ownership stay outside
//! this module; a caller must complete a fresh observation before preparing one.
use super::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, io::Write, mem::size_of};

pub(super) const MAX_BASELINE: usize = 256 * 1024;
const MAX_CONSTRUCTION: usize = 512 * 1024;
const TTL: Duration = Duration::from_secs(60);
const RETAINED: usize = 2;
// Two bounded sessions give at most 1 MiB, 4,000 node indices and 128 frame
// slots. Increasing session capacity requires a separate shared cache budget.
const _: () = assert!(MAX_SESSIONS <= 2);

#[derive(Clone, Copy)]
struct Span {
    start: u32,
    end: u32,
}
impl Span {
    fn bytes(self, source: &[u8]) -> &[u8] {
        &source[self.start as usize..self.end as usize]
    }
}

struct Encoded {
    bytes: Vec<u8>,
    nodes: Vec<Span>,
    metadata: usize,
    frames: usize,
}
pub(super) struct Baseline {
    revision: String,
    encoded: Encoded,
    created: Instant,
}
pub(super) struct Prepared {
    sequence: u64,
    key: [u8; 32],
    baseline: Option<Baseline>,
}

#[derive(Default)]
pub(super) struct Cache {
    sequence: u64,
    key: Option<[u8; 32]>,
    baselines: VecDeque<Baseline>,
    #[cfg(test)]
    construction_peak: usize,
    #[cfg(test)]
    preparation_nanos: u64,
    #[cfg(test)]
    capture_oracle: bool,
    #[cfg(test)]
    oracle: Option<Value>,
}

impl Cache {
    pub fn clear(&mut self) {
        self.baselines.clear();
        self.key = None;
    }

    pub fn expire(&mut self, now: Instant) {
        self.baselines
            .retain(|entry| now.saturating_duration_since(entry.created) < TTL);
    }

    /// Called synchronously while the owning session is locked. Publication is
    /// separate so preparation cannot cross a changed control epoch.
    pub fn prepare(
        &mut self,
        value: &mut Value,
        frames: &HashMap<String, sem::Context>,
        key: [u8; 32],
        base_revision: Option<&str>,
        now: Instant,
    ) -> Result<Prepared> {
        #[cfg(test)]
        if self.capture_oracle {
            self.oracle = Some(value.clone());
        }
        #[cfg(test)]
        let preparation_started = Instant::now();
        self.expire(now);
        if self.key != Some(key) {
            self.clear();
            self.key = Some(key);
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(sem::types::decode)?;
        let revision = token()?;
        let nodes = value["nodes"]
            .as_array_mut()
            .ok_or_else(sem::types::decode)?;
        if nodes.len() > sem::types::MAX_NODES || frames.len() > sem::types::MAX_FRAMES {
            return Err(sem::types::decode());
        }
        // Context is an internal identity, independent of randomly issued IDs.
        let mut order: Vec<_> = frames.iter().collect();
        order.sort_by_key(|(_, context)| **context);
        let mut references = Vec::with_capacity(nodes.len());
        let mut complete_records = true;
        for node in nodes.iter_mut() {
            let object = node.as_object_mut().ok_or_else(sem::types::decode)?;
            let frame = object.remove("frame_id").ok_or_else(sem::types::decode)?;
            let slot = order
                .iter()
                .position(|(id, _)| frame.as_str() == Some(id.as_str()))
                .ok_or_else(sem::types::decode)?;
            object.insert("frame_slot".into(), slot.into());
            references.push(object.remove("ref").unwrap_or(Value::Null));
            complete_records &= object.get("record_complete") == Some(&Value::Bool(true))
                && object.get("record").is_some_and(|record| {
                    record["complete"] == true
                        && record["limits"].as_array().is_some_and(Vec::is_empty)
                });
        }
        let node_count = nodes.len();
        let frame_ids: Vec<_> = order
            .iter()
            .map(|(id, _)| Value::String((*id).clone()))
            .collect();
        let binding_allocation = references.capacity() * size_of::<Value>()
            + references.iter().map(|value| match value { Value::String(s) => s.capacity(), _ => 0 }).sum::<usize>()
            + frame_ids.capacity() * size_of::<Value>()
            + order.capacity() * size_of::<(&String, &sem::Context)>()
            + frame_ids
                .iter()
                .filter_map(Value::as_str)
                .map(str::len)
                .sum::<usize>()
            + node_count * "frame_slot".len()
            // Bounded maps, scalar fields and newly issued revision metadata.
            + 16 * 1024;
        let cacheable = value["coverage"]["complete"] == true
            && value["coverage"]["changed_during_read"] == false
            && value.get("next_cursor").is_none()
            && complete_records;
        let mut encoded = if cacheable {
            encode(value, frames.len())?
        } else {
            None
        };
        let construction = binding_allocation
            + encoded.as_ref().map_or(0, |entry| {
                entry.bytes.capacity() + entry.nodes.capacity() * size_of::<Span>()
            });
        #[cfg(test)]
        {
            self.construction_peak = self.construction_peak.max(construction);
        }
        if construction > MAX_CONSTRUCTION {
            encoded = None;
        }
        let mut candidate = None;
        if let Some(current) = &encoded
            && let Some(previous) = base_revision
                .and_then(|base| self.baselines.iter().find(|entry| entry.revision == base))
        {
            let old = &previous.encoded;
            let mut prefix = 0;
            while prefix < old.nodes.len().min(current.nodes.len())
                && old.nodes[prefix].bytes(&old.bytes)
                    == current.nodes[prefix].bytes(&current.bytes)
            {
                prefix += 1;
            }
            let mut suffix = 0;
            while suffix < old.nodes.len().min(current.nodes.len()) - prefix
                && old.nodes[old.nodes.len() - suffix - 1].bytes(&old.bytes)
                    == current.nodes[current.nodes.len() - suffix - 1].bytes(&current.bytes)
            {
                suffix += 1;
            }
            candidate = Some(json!({"version":1,"base_revision":previous.revision,
                "base_count":old.nodes.len(),"start":prefix,
                "delete_count":old.nodes.len()-prefix-suffix,
                "insert_count":current.nodes.len()-prefix-suffix,
                "unchanged":prefix==old.nodes.len()&&prefix==current.nodes.len()
                    &&old.bytes[old.metadata..]==current.bytes[current.metadata..]}));
        }
        if encoded.is_none() {
            self.baselines.clear();
        }
        let object = value.as_object_mut().ok_or_else(sem::types::decode)?;
        let nodes = object.remove("nodes").ok_or_else(sem::types::decode)?;
        let mut bindings = json!({});
        bindings["references"] = references.into();
        bindings["frames"] = frame_ids.into();
        if let Some(cursor) = object.remove("next_cursor") {
            bindings["next_cursor"] = cursor;
        }
        let mut presentation = json!({"version":1,"kind":"full","revision":revision,
            "sequence":sequence.to_string(),"node_count":node_count,
            "cacheable":encoded.is_some()});
        presentation["bindings"] = bindings;
        presentation["nodes"] = nodes;
        if let Some(candidate) = candidate {
            presentation["_candidate"] = candidate;
        }
        object.insert("presentation".into(), presentation);
        self.sequence = sequence;
        if self.baselines.capacity() < RETAINED {
            self.baselines
                .reserve_exact(RETAINED - self.baselines.len());
        }
        #[cfg(test)]
        {
            self.preparation_nanos = preparation_started
                .elapsed()
                .as_nanos()
                .min(u64::MAX as u128) as u64;
        }
        Ok(Prepared {
            sequence,
            key,
            baseline: encoded.map(|encoded| Baseline {
                revision,
                encoded,
                created: now,
            }),
        })
    }

    /// The caller holds the final control fence as well as the session lock.
    /// Return evicted storage so deallocation can occur outside the fence.
    pub fn publish(&mut self, prepared: Prepared) -> Result<Option<Baseline>> {
        if prepared.sequence != self.sequence || self.key != Some(prepared.key) {
            return Err(sem::types::decode());
        }
        let Some(baseline) = prepared.baseline else {
            return Ok(None);
        };
        if baseline.encoded.bytes.len() > MAX_BASELINE
            || baseline.encoded.nodes.len() > sem::types::MAX_NODES
            || baseline.encoded.frames > sem::types::MAX_FRAMES
        {
            return Err(sem::types::decode());
        }
        let retired = if self.baselines.len() == RETAINED {
            self.baselines.pop_front()
        } else {
            None
        };
        self.baselines.push_back(baseline);
        Ok(retired)
    }

    #[cfg(test)]
    pub fn metrics(&self) -> Value {
        json!({"entries":self.baselines.len(),
            "encoded_bytes":self.baselines.iter().map(|b|b.encoded.bytes.len()).sum::<usize>(),
            "allocated_bytes":self.baselines.iter().map(|b|b.encoded.bytes.capacity()).sum::<usize>(),
            "node_indices":self.baselines.iter().map(|b|b.encoded.nodes.len()).sum::<usize>(),
            "index_bytes":self.baselines.iter().map(|b|b.encoded.nodes.capacity()*size_of::<Span>()).sum::<usize>(),
            "frame_slots":self.baselines.iter().map(|b|b.encoded.frames).sum::<usize>(),
            "construction_peak":self.construction_peak,
            "preparation_nanos":self.preparation_nanos})
    }
}

/// Hash only verified internal identities, never user-provided native IDs.
pub(super) fn key(
    frames: &[sem::Frame],
    query: &sem::Query,
    tab: &str,
    epoch: u64,
    nodes: usize,
    characters: usize,
) -> Result<[u8; 32]> {
    struct Hash(Sha256);
    impl Write for Hash {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut order: Vec<_> = frames.iter().collect();
    order.sort_by_key(|frame| frame.context);
    let mut hash = Hash(Sha256::new());
    serde_json::to_writer(&mut hash, &(order, query, tab, epoch, nodes, characters))
        .map_err(|_| sem::types::decode())?;
    Ok(hash.0.finalize().into())
}

struct Writer {
    bytes: Option<Vec<u8>>,
    length: usize,
    limit: usize,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.length) {
            return Err(std::io::Error::other("display projection byte limit"));
        }
        if let Some(output) = &mut self.bytes {
            output.extend_from_slice(bytes);
        }
        self.length += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn emit(writer: &mut Writer, value: &impl Serialize) -> std::io::Result<()> {
    serde_json::to_writer(writer, value).map_err(std::io::Error::other)
}
fn projection(
    writer: &mut Writer,
    value: &Value,
    mut spans: Option<&mut Vec<Span>>,
) -> std::io::Result<usize> {
    writer.write_all(b"{\"nodes\":[")?;
    for (index, node) in value["nodes"].as_array().unwrap().iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        let start = writer.length;
        emit(writer, node)?;
        if let Some(spans) = &mut spans {
            spans.push(Span {
                start: start as u32,
                end: writer.length as u32,
            });
        }
    }
    let metadata = writer.length;
    writer.write_all(b"],\"page\":")?;
    emit(writer, &value["page"])?;
    writer.write_all(b",\"coverage\":")?;
    emit(writer, &value["coverage"])?;
    writer.write_all(b",\"observation_profile\":")?;
    emit(writer, &value["observation_profile"])?;
    writer.write_all(b",\"content_trust\":")?;
    emit(writer, &value["content_trust"])?;
    writer.write_all(b"}")?;
    Ok(metadata)
}
fn encode(value: &Value, frames: usize) -> Result<Option<Encoded>> {
    let mut count = Writer {
        bytes: None,
        length: 0,
        limit: MAX_BASELINE,
    };
    if projection(&mut count, value, None).is_err() {
        return Ok(None);
    }
    let mut writer = Writer {
        bytes: Some(Vec::with_capacity(count.length)),
        length: 0,
        limit: count.length,
    };
    let mut spans = Vec::with_capacity(value["nodes"].as_array().unwrap().len());
    let metadata =
        projection(&mut writer, value, Some(&mut spans)).map_err(|_| sem::types::decode())?;
    if writer.length != count.length {
        return Err(sem::types::decode());
    }
    Ok(Some(Encoded {
        bytes: writer.bytes.unwrap(),
        nodes: spans,
        metadata,
        frames,
    }))
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
