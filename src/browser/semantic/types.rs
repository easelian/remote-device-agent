use crate::browser::{Failure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) const MAX_FRAMES: usize = 32;
pub(crate) const MAX_NODES: usize = 1000;
pub(crate) const MAX_CHARACTERS: usize = 64_000;
/// A compact read may be asked for more, so that a long table is read in one call.
pub(crate) const MAX_COMPACT_NODES: usize = 10_000;
pub(crate) const MAX_COMPACT_CHARACTERS: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct Context {
    scope: u64,
    index: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Frame {
    pub context: Context,
    pub parent: Option<Context>,
    pub document: u64,
    pub realm: u64,
    pub url: String,
    pub(crate) marker: Marker,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Marker {
    pub realm: String,
    pub restoration: u64,
    pub url: String,
}
impl Marker {
    pub fn validate(&self) -> Result<()> {
        if self.realm.is_empty() || self.realm.len() > 128 || self.url.len() > 8192 {
            return Err(decode());
        }
        Ok(())
    }
}

/// One adapter-owned identity table. Native keys never leave the adapter.
pub(crate) struct Identities {
    scope: u64,
    next: u64,
    entries: HashMap<String, Identity>,
}
struct Identity {
    context: Context,
    parent: Option<Context>,
    document_key: String,
    realm_key: String,
    marker: Marker,
    document: u64,
    realm: u64,
}
static SCOPES: AtomicU64 = AtomicU64::new(1);
impl Identities {
    pub fn new() -> Self {
        Self {
            scope: SCOPES.fetch_add(1, Ordering::Relaxed),
            next: 0,
            entries: HashMap::new(),
        }
    }
    pub fn context(&mut self, key: &str) -> Result<Context> {
        if let Some(entry) = self.entries.get(key) {
            return Ok(entry.context);
        }
        if key.is_empty() || key.len() > 512 || self.entries.len() >= MAX_FRAMES {
            return Err(Failure::new("browser_frame_limit"));
        }
        self.next = self.next.checked_add(1).ok_or_else(decode)?;
        let context = Context {
            scope: self.scope,
            index: self.next,
        };
        self.entries.insert(
            key.into(),
            Identity {
                context,
                parent: None,
                document_key: String::new(),
                realm_key: String::new(),
                marker: Marker {
                    realm: String::new(),
                    restoration: 0,
                    url: String::new(),
                },
                document: 0,
                realm: 0,
            },
        );
        Ok(context)
    }
    pub fn key(&self, context: Context) -> Result<&str> {
        if context.scope != self.scope {
            return Err(stale());
        }
        self.entries
            .iter()
            .find(|(_, entry)| entry.context == context)
            .map(|(key, _)| key.as_str())
            .ok_or_else(stale)
    }
    pub fn retain(&mut self, keys: &[String]) {
        self.entries.retain(|key, _| keys.contains(key));
    }
    pub fn parent(&self, context: Context) -> Result<Option<Context>> {
        let key = self.key(context)?;
        Ok(self.entries.get(key).ok_or_else(stale)?.parent)
    }
    pub fn observe(
        &mut self,
        key: &str,
        parent: Option<Context>,
        document_key: &str,
        realm_key: &str,
        marker: Marker,
    ) -> Result<Frame> {
        marker.validate()?;
        if document_key.is_empty() || realm_key.is_empty() {
            return Err(decode());
        }
        let context = self.context(key)?;
        let entry = self.entries.get_mut(key).unwrap();
        entry.parent = parent;
        // A new realm alone does not fabricate navigation. A persisted page
        // restoration fences the old document even when native IDs are reused.
        if entry.document_key != document_key
            || (entry.marker.realm == marker.realm
                && entry.marker.restoration != marker.restoration)
        {
            entry.document = entry.document.checked_add(1).ok_or_else(decode)?;
        }
        if entry.realm_key != realm_key || entry.marker.realm != marker.realm {
            entry.realm = entry.realm.checked_add(1).ok_or_else(decode)?;
        }
        entry.document_key = document_key.into();
        entry.realm_key = realm_key.into();
        entry.marker = marker.clone();
        Ok(Frame {
            context,
            parent,
            document: entry.document,
            realm: entry.realm,
            url: marker.url.clone(),
            marker,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter {
    #[default]
    All,
    Interactive,
    Viewport,
    DragEndpoints,
    /// Controls, and with them the smallest elements that hold the words looked for.
    Text,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Query {
    pub filter: Filter,
    pub find: Option<String>,
    pub within: Option<Context>,
    /// Budget rows by their compact public cost. The page facts are unchanged.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub compact: bool,
    /// Read only the parts of each document altered since they were last read,
    /// and keep the references already issued.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub changed: bool,
}
impl Query {
    pub fn validate(&self) -> Result<()> {
        if self
            .find
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.chars().count() > 512)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub identity: u64,
    pub tag: String,
    pub attributes: Vec<(String, String)>,
    pub text: String,
    pub value: Option<String>,
    pub checked: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Material {
    // Private identity of the list that owns this option, included in its action fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_list: Option<u64>,
    // Enclosing record material binds nested actions across row moves and recycling.
    // The MCP projection exports only entries from the target's own record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_context: Vec<Entry>,
    pub entries: Vec<Entry>,
    pub complete: bool,
    pub limits: Vec<String>,
    // The page program sends the material of a record with the first of its
    // rows in a batch. A later row names that row by its position and is
    // given the same material before anything reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Facts {
    pub node: u64,
    pub id: String,
    pub role: String,
    pub kind: String,
    pub name: String,
    pub name_source: String,
    pub name_quality: Vec<String>,
    pub value: Option<String>,
    pub enabled: bool,
    pub readonly: bool,
    pub editable: bool,
    pub sensitive: bool,
    pub checked: bool,
    pub tag: String,
    pub input_type: String,
    pub visible: bool,
    pub hit: bool,
    pub x: f64,
    pub y: f64,
    pub in_view: bool,
    pub record: Material,
}
impl Facts {
    pub fn validate(&self) -> Result<()> {
        if self.node == 0
            || self.id.len() > 8192
            || self.role.len() > 64
            || self.kind.len() > 32
            || self.name.chars().count() > 512
            || self.name_source.len() > 64
            || self.name_quality.len() > 16
            || self.name_quality.iter().any(|s| s.len() > 64)
            || self
                .value
                .as_ref()
                .is_some_and(|v| v.chars().count() > 4096)
            || self.sensitive && self.value.is_some()
            || !self.x.is_finite()
            || !self.y.is_finite()
            || self.record.owner_list == Some(0)
            || self.record.entries.len() + self.record.owner_context.len() > 256
            || self.record.limits.len() > 16
            || self.record.complete && !self.record.limits.is_empty()
        {
            return Err(decode());
        }
        let mut chars = 0;
        for entry in self.record.entries.iter().chain(&self.record.owner_context) {
            if entry.identity == 0 || entry.tag.len() > 64 || entry.attributes.len() > 40 {
                return Err(decode());
            }
            chars +=
                entry.text.chars().count() + entry.value.as_ref().map_or(0, |v| v.chars().count());
            for (key, value) in &entry.attributes {
                if key.len() > 64 {
                    return Err(decode());
                }
                chars += value.chars().count();
            }
        }
        if chars > 4096 {
            return Err(decode());
        }
        Ok(())
    }
    pub fn eligible_for(&self, filter: &Filter) -> bool {
        if *filter == Filter::DragEndpoints {
            return self.endpoint_eligible();
        }
        self.eligible()
    }
    pub fn endpoint_eligible(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "drag_source" | "pointer_source" | "drop_target"
        ) && self.enabled
            && self.visible
            && !self.sensitive
            && !self.editable
            && !self.name.is_empty()
            && self.name_quality.is_empty()
            && self.record.complete
            && self.node != 0
            && self.input_type != "file"
    }
    pub fn eligible(&self) -> bool {
        self.enabled
            && self.visible
            && self.record.complete
            && self.name_quality.is_empty()
            && (matches!(self.kind.as_str(), "native" | "aria") && interactive(&self.role)
                || self.kind == "inferred"
                || self.kind == "scrollable")
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        // Canonical fields and record material are serialized from validated
        // types in a fixed field order. Geometry is rechecked independently.
        let bytes = serde_json::to_vec(&(
            &self.node,
            &self.role,
            &self.kind,
            &self.name,
            &self.name_source,
            &self.name_quality,
            &self.value,
            self.enabled,
            self.readonly,
            self.editable,
            self.sensitive,
            self.checked,
            &self.tag,
            &self.input_type,
            &self.record,
        ))
        .expect("serializable facts");
        Sha256::digest(bytes).into()
    }
}
pub(crate) fn interactive(role: &str) -> bool {
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
            | "treeitem"
    )
}

impl Batch {
    /// Give every row that names an earlier row the material of that row.
    pub fn share(&mut self) -> Result<()> {
        for index in 0..self.rows.len() {
            let Some(same) = self.rows[index].record.same.take() else {
                continue;
            };
            let same = same as usize;
            if same >= index || !self.rows[index].record.entries.is_empty() {
                return Err(decode());
            }
            self.rows[index].record = self.rows[same].record.clone();
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Batch {
    pub revision: u64,
    pub stamp: Marker,
    pub rows: Vec<Facts>,
    pub sequence: u64,
    pub more: bool,
    pub scanned: usize,
    pub changed: bool,
    pub omitted: Vec<String>,
    pub characters: usize,
    /// Elements read so far and elements in the document, for a compact read.
    #[serde(default)]
    pub progress: Option<[u32; 2]>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub visible: bool,
    pub hit: bool,
    pub in_view: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryStats {
    pub nodes: usize,
    pub scans: usize,
    pub pending: usize,
}
impl RegistryStats {
    pub fn validate(&self) -> Result<()> {
        // The largest compact read, and what a view after an action may add to it.
        if self.nodes > MAX_COMPACT_NODES + 2 * MAX_NODES || self.scans > 1 || self.pending > 50000
        {
            return Err(decode());
        }
        Ok(())
    }
}
impl Geometry {
    pub fn validate(&self) -> Result<()> {
        if [self.x, self.y, self.width, self.height]
            .iter()
            .any(|v| !v.is_finite())
            || self.width <= 0.
            || self.height <= 0.
        {
            return Err(decode());
        }
        Ok(())
    }
}
impl Batch {
    pub fn validate(&self, nodes: usize, characters: usize) -> Result<()> {
        self.stamp.validate()?;
        if self.rows.len() > nodes
            || self.characters > characters
            || self.scanned > 50000
            || self.omitted.len() > 32
        {
            return Err(decode());
        }
        for row in &self.rows {
            row.validate()?;
        }
        Ok(())
    }
}

/// How the pointer is pressed on what it clicks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Manner {
    #[default]
    Once,
    /// Twice in a row, as opens an item or starts editing it in place.
    Twice,
    /// With the other button, as opens a menu of the page's own.
    Other,
}
#[derive(Clone, Debug)]
pub(crate) enum NativeInput {
    SelectFile {
        node: u64,
        path: std::path::PathBuf,
    },
    Click {
        node: u64,
        // Set only for the one-use continuation of a native radio selection.
        reuse_radio: Option<u64>,
        x: f64,
        y: f64,
        manner: Manner,
    },
    ReplaceText(String),
    Press(Key),
    Hover {
        node: u64,
    },
    Drag {
        source: super::DragEndpoint,
        target: super::DragEndpoint,
    },
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Key {
    Enter,
    Tab,
    Escape,
    Backspace,
    ArrowDown,
    ArrowUp,
    ArrowLeft,
    ArrowRight,
}

impl Key {
    pub fn name(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Tab => "Tab",
            Self::Escape => "Escape",
            Self::Backspace => "Backspace",
            Self::ArrowDown => "ArrowDown",
            Self::ArrowUp => "ArrowUp",
            Self::ArrowLeft => "ArrowLeft",
            Self::ArrowRight => "ArrowRight",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "Enter" => Self::Enter,
            "Tab" => Self::Tab,
            "Escape" => Self::Escape,
            "Backspace" => Self::Backspace,
            "ArrowDown" => Self::ArrowDown,
            "ArrowUp" => Self::ArrowUp,
            "ArrowLeft" => Self::ArrowLeft,
            "ArrowRight" => Self::ArrowRight,
            _ => return Err(Failure::new("browser_invalid_params")),
        })
    }
    pub fn bidi(self) -> &'static str {
        match self {
            Self::Enter => "\u{e007}",
            Self::Tab => "\u{e004}",
            Self::Escape => "\u{e00c}",
            Self::Backspace => "\u{e003}",
            Self::ArrowDown => "\u{e015}",
            Self::ArrowUp => "\u{e013}",
            Self::ArrowLeft => "\u{e012}",
            Self::ArrowRight => "\u{e014}",
        }
    }
}

pub(crate) fn stale() -> Failure {
    Failure::new("browser_stale_reference")
}
pub(crate) fn decode() -> Failure {
    Failure::new("browser_semantic_decode_failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_realm_restoration_and_context_lifetimes_are_distinct() {
        let mut ids = Identities::new();
        let mark = |realm: &str, restoration, url: &str| Marker {
            realm: realm.into(),
            restoration,
            url: url.into(),
        };
        let first = ids
            .observe("a", None, "doc", "world", mark("r", 0, "http://fixture/"))
            .unwrap();
        let hash = ids
            .observe("a", None, "doc", "world", mark("r", 0, "http://fixture/#x"))
            .unwrap();
        assert_eq!((first.document, first.realm), (hash.document, hash.realm));
        let realm = ids
            .observe(
                "a",
                None,
                "doc",
                "world2",
                mark("r2", 0, "http://fixture/#x"),
            )
            .unwrap();
        assert_eq!(first.document, realm.document);
        assert_ne!(first.realm, realm.realm);
        let restored = ids
            .observe("a", None, "doc", "world2", mark("r2", 1, "http://fixture/"))
            .unwrap();
        assert_ne!(realm.document, restored.document);
        ids.retain(&[]);
        let replacement = ids.context("a").unwrap();
        assert_ne!(first.context, replacement);
        assert!(ids.key(first.context).is_err());
        assert!(Identities::new().key(replacement).is_err());
    }
}
