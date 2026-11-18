//! Only the first three synchronous frames enter the capture. Raw source keys
//! never leave the native notification, and public ordinals apply to one window.
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

const MAX_FRAMES: usize = 3;
const MAX_SOURCES: usize = 32;
const MAX_SOURCE_BYTES: usize = 2048;
type DigestKey = [u8; 32];

#[derive(Serialize)]
struct PublicFrame {
    source_id: Option<u8>,
    line: u32,
    column: u32,
}

struct Location {
    sequence: u64,
    frames: Vec<PublicFrame>,
    truncated: bool,
}

#[derive(Default)]
pub(super) struct Locations {
    sources: Vec<DigestKey>,
    entries: VecDeque<Location>,
}

impl Locations {
    pub(super) fn forget_sources(&mut self) {
        self.sources = Vec::new();
    }

    fn ordinal(&mut self, key: DigestKey) -> Option<u8> {
        if let Some(index) = self.sources.iter().position(|known| *known == key) {
            return Some(index as u8 + 1);
        }
        if self.sources.len() == MAX_SOURCES {
            return None;
        }
        self.sources.push(key);
        Some(self.sources.len() as u8)
    }

    pub(super) fn push(&mut self, sequence: u64, stack: &Value, source_field: &str) {
        let native = stack["callFrames"].as_array();
        let items = native.map(Vec::as_slice).unwrap_or_default();
        let mut truncated = native.is_none()
            || items.len() > MAX_FRAMES
            || ["parent", "parentId", "asyncStackTrace"]
                .iter()
                .any(|key| !stack[*key].is_null());
        let mut frames = Vec::with_capacity(items.len().min(MAX_FRAMES));
        for item in items.iter().take(MAX_FRAMES) {
            let (Some(line), Some(column)) = (
                position(&item["lineNumber"]),
                position(&item["columnNumber"]),
            ) else {
                truncated = true;
                continue;
            };
            let source_id = item[source_field]
                .as_str()
                .filter(|key| !key.is_empty() && key.len() <= MAX_SOURCE_BYTES)
                .and_then(|key| self.ordinal(Sha256::digest(key.as_bytes()).into()));
            frames.push(PublicFrame {
                source_id,
                line,
                column,
            });
        }
        self.entries.push_back(Location {
            sequence,
            frames,
            truncated,
        });
    }

    pub(super) fn evict(&mut self, sequence: u64) {
        if self
            .entries
            .front()
            .is_some_and(|first| first.sequence == sequence)
        {
            self.entries.pop_front();
        }
    }

    pub(super) fn get(&self, sequence: u64) -> Option<Value> {
        let location = self.entries.iter().find(|row| row.sequence == sequence)?;
        Some(json!({"sequence":sequence, "frames":location.frames, "truncated":location.truncated}))
    }
}

/// CDP and BiDi native call frames both use zero-based positions.
fn position(value: &Value) -> Option<u32> {
    let n = value.as_u64()?;
    (n < i32::MAX as u64).then(|| n as u32 + 1)
}
