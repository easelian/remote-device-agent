//! Bounded DOM facts, independent of canonical observation and action refs.
use super::{
    Context, Frame, Port,
    program::{Output, Program},
    types,
};
use crate::browser::{Failure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const SOURCE: &str = include_str!("evidence.js");
pub(crate) const TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const MAX_ELEMENTS: usize = 5_000;
pub(crate) const MAX_TEXT_NODES: usize = 10_000;
pub(crate) const MAX_OTHER_NODES: usize = 5_000;
pub(crate) const MAX_RESULTS: usize = 64;
pub(crate) const MAX_BYTES: usize = 16 * 1024;
pub(crate) const ATTRIBUTES: &[&str] = &[
    "type",
    "aria-hidden",
    "aria-disabled",
    "aria-expanded",
    "aria-selected",
    "hidden",
    "inert",
    "disabled",
    "readonly",
];
pub(crate) const STYLES: &[&str] = &[
    "display",
    "visibility",
    "opacity",
    "pointer-events",
    "position",
    "z-index",
    "overflow-x",
    "overflow-y",
    "width",
    "height",
    "transform",
    "font-size",
    "line-height",
    "color",
    "background-color",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scope {
    #[default]
    MainDocument,
    AllFrames,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Query {
    FindNodes {
        tag: Option<String>,
        role: Option<String>,
        text: Option<String>,
        #[serde(default)]
        scope: Scope,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    ReadNode {
        node_id: String,
    },
    ReadLayout {
        node_id: String,
    },
    ReadAccessibility {
        node_id: String,
    },
    ReadValidation {
        node_id: String,
    },
    // Internal-only query used by record_wait after resolving an evidence binding.
    #[serde(skip_deserializing)]
    MatchRecord {
        node_id: String,
        anchor_text: String,
        expected_text: String,
    },
}
fn default_limit() -> usize {
    16
}
fn invalid() -> Failure {
    Failure::new("browser_invalid_params")
}
fn identifier(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
pub(crate) fn node_id(value: &str) -> bool {
    value.len() == 36
        && value.starts_with("den_")
        && value[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Query {
    pub fn parse(value: &Value) -> Result<Self> {
        if value
            .as_object()
            .is_none_or(|map| map.values().any(Value::is_null))
        {
            return Err(invalid());
        }
        let query: Self = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        match &query {
            Self::FindNodes {
                tag,
                role,
                text,
                limit,
                ..
            } => {
                if tag.is_none() && role.is_none() && text.is_none()
                    || tag.as_ref().is_some_and(|v| !identifier(v))
                    || role.as_ref().is_some_and(|v| !identifier(v))
                    || text
                        .as_ref()
                        .is_some_and(|v| v.is_empty() || v.chars().count() > 128)
                    || !(1..=MAX_RESULTS).contains(limit)
                {
                    return Err(invalid());
                }
            }
            Self::ReadNode { node_id: id }
            | Self::ReadLayout { node_id: id }
            | Self::ReadAccessibility { node_id: id }
            | Self::ReadValidation { node_id: id } => {
                if !node_id(id) {
                    return Err(invalid());
                }
            }
            // This variant is intentionally unavailable to serde input; only
            // record_wait may construct it after resolving an evidence binding.
            Self::MatchRecord { .. } => return Err(invalid()),
        }
        Ok(query)
    }
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::FindNodes { .. } => None,
            Self::ReadNode { node_id }
            | Self::ReadLayout { node_id }
            | Self::ReadAccessibility { node_id }
            | Self::ReadValidation { node_id }
            | Self::MatchRecord { node_id, .. } => Some(node_id),
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::FindNodes { .. } => "find_nodes",
            Self::ReadNode { .. } => "read_node",
            Self::ReadLayout { .. } => "read_layout",
            Self::ReadAccessibility { .. } => "read_accessibility",
            Self::ReadValidation { .. } => "read_validation",
            Self::MatchRecord { .. } => "match_record",
        }
    }
    pub fn native_ax(&self) -> bool {
        matches!(self, Self::ReadAccessibility { .. })
    }
    pub fn source(&self) -> &'static str {
        if self.native_ax() { "native_ax" } else { "dom" }
    }
    pub fn scope(&self) -> &'static str {
        match self {
            Self::FindNodes {
                scope: Scope::AllFrames,
                ..
            } => "all_frames",
            Self::FindNodes { .. } => "main_document",
            _ => "node",
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct NativeQuery {
    pub kind: &'static str,
    pub tag: Option<String>,
    pub role: Option<String>,
    pub text: Option<String>,
    pub node: Option<u64>,
    pub limit: usize,
    pub elements: usize,
    pub text_nodes: usize,
    pub other_nodes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_text: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Limit {
    #[serde(rename = "element_limit")]
    Element,
    #[serde(rename = "text_node_limit")]
    TextNode,
    #[serde(rename = "other_node_limit")]
    OtherNode,
    #[serde(rename = "depth_limit")]
    Depth,
    #[serde(rename = "result_limit")]
    Result,
    #[serde(rename = "name_limit")]
    Name,
    #[serde(rename = "excerpt_limit")]
    Excerpt,
    #[serde(rename = "attribute_limit")]
    Attribute,
    #[serde(rename = "style_limit")]
    Style,
    #[serde(rename = "byte_limit")]
    Byte,
    #[serde(rename = "ax_name_limit")]
    AxName,
    #[serde(rename = "ax_role_limit")]
    AxRole,
    #[serde(rename = "ax_property_limit")]
    AxProperty,
    #[serde(rename = "ax_reason_limit")]
    AxReason,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Omitted {
    ClosedShadowRoots,
    CanvasContent,
    BrowserUi,
    AncestorFrameHitTesting,
    FormValues,
    EditableText,
    UnresolvedNameReference,
    NativeAxTree,
    NativeAxValue,
    NativeAxDescription,
    NativeAxRelations,
    NativeAxOtherProperties,
    NativeAxName,
    UnrecognizedAxProperty,
    UnrecognizedAxReason,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Coverage {
    pub visited_elements: usize,
    pub visited_text_nodes: usize,
    pub visited_other_nodes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_ax_nodes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_ax_properties: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_ax_ignored_reasons: Option<usize>,
    pub truncated: bool,
    pub limits: BTreeSet<Limit>,
    pub omitted: BTreeSet<Omitted>,
}
impl Coverage {
    pub fn base() -> Self {
        Self {
            omitted: [
                Omitted::ClosedShadowRoots,
                Omitted::CanvasContent,
                Omitted::BrowserUi,
                Omitted::AncestorFrameHitTesting,
                Omitted::FormValues,
                Omitted::EditableText,
            ]
            .into(),
            ..Default::default()
        }
    }
    fn merge(&mut self, other: &Self) {
        self.visited_elements += other.visited_elements;
        self.visited_text_nodes += other.visited_text_nodes;
        self.visited_other_nodes += other.visited_other_nodes;
        if let Some(n) = other.native_ax_nodes {
            self.native_ax_nodes = Some(self.native_ax_nodes.unwrap_or(0) + n);
        }
        if let Some(n) = other.native_ax_properties {
            self.native_ax_properties = Some(self.native_ax_properties.unwrap_or(0) + n);
        }
        if let Some(n) = other.native_ax_ignored_reasons {
            self.native_ax_ignored_reasons = Some(self.native_ax_ignored_reasons.unwrap_or(0) + n);
        }
        self.truncated |= other.truncated;
        self.limits.extend(&other.limits);
        self.omitted.extend(&other.omitted);
    }
    pub fn limit(&mut self, reason: Limit) {
        self.truncated = true;
        self.limits.insert(reason);
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Node {
    pub node: u64,
    pub tag: String,
    pub role: String,
    pub name: String,
    pub name_source: NameSource,
    pub sensitive: bool,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NameSource {
    AriaLabelledby,
    AriaLabel,
    Label,
    DirectText,
    None,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct States {
    pub connected: bool,
    pub disabled: bool,
    pub readonly: bool,
    pub editable: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Detail {
    pub facts: Node,
    pub text_excerpt: String,
    pub states: States,
    pub attributes: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Viewport {
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HitKind {
    Target,
    Descendant,
    Other,
    OutsideViewport,
    Indeterminate,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hit {
    pub kind: HitKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Layout {
    pub r#box: Rect,
    pub viewport: Viewport,
    pub computed: BTreeMap<String, String>,
    pub hit: Hit,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationState {
    Valid,
    Invalid,
    NotApplicable,
    Omitted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationReason {
    ValueMissing,
    TypeMismatch,
    PatternMismatch,
    TooLong,
    TooShort,
    RangeUnderflow,
    RangeOverflow,
    StepMismatch,
    BadInput,
    CustomError,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Validation {
    pub state: ValidationState,
    pub reasons: Vec<ValidationReason>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Raw {
    FindNodes {
        nodes: Vec<Node>,
        coverage: Coverage,
    },
    ReadNode {
        node: Detail,
        coverage: Coverage,
    },
    ReadLayout {
        layout: Layout,
        coverage: Coverage,
    },
    ReadAccessibility {
        accessibility: super::accessibility::Snapshot,
        coverage: Coverage,
    },
    ReadValidation {
        validation: Validation,
        coverage: Coverage,
    },
    MatchRecord {
        matched: bool,
        coverage: Coverage,
    },
}
fn short(value: &str, maximum: usize) -> bool {
    value.chars().count() <= maximum
}
fn serial(value: u64) -> bool {
    value > 0 && value <= 9_007_199_254_740_991
}
impl Node {
    fn valid(&self) -> bool {
        serial(self.node)
            && !self.tag.is_empty()
            && short(&self.tag, 64)
            && short(&self.role, 64)
            && short(&self.name, 256)
    }
}
impl Raw {
    pub fn coverage(&self) -> &Coverage {
        match self {
            Self::FindNodes { coverage, .. }
            | Self::ReadNode { coverage, .. }
            | Self::ReadLayout { coverage, .. }
            | Self::ReadAccessibility { coverage, .. }
            | Self::ReadValidation { coverage, .. }
            | Self::MatchRecord { coverage, .. } => coverage,
        }
    }
    pub fn decode(text: &str, query: &NativeQuery) -> Result<Self> {
        if text.len() > MAX_BYTES {
            return Err(types::decode());
        }
        let raw: Self = serde_json::from_str::<Option<Self>>(text)
            .map_err(|_| types::decode())?
            .ok_or_else(types::stale)?;
        let coverage = raw.coverage();
        let valid = coverage.native_ax_nodes.is_none()
            && coverage.native_ax_properties.is_none()
            && coverage.native_ax_ignored_reasons.is_none()
            && coverage.visited_elements <= query.elements
            && coverage.visited_text_nodes <= query.text_nodes
            && coverage.visited_other_nodes <= query.other_nodes
            && coverage.truncated != coverage.limits.is_empty();
        let shape = match &raw {
            Self::FindNodes { nodes, .. } => {
                query.kind == "find_nodes"
                    && nodes.len() <= query.limit
                    && nodes.iter().all(Node::valid)
                    && nodes.iter().map(|n| n.node).collect::<BTreeSet<_>>().len() == nodes.len()
            }
            Self::ReadNode { node, .. } => {
                query.kind == "read_node"
                    && Some(node.facts.node) == query.node
                    && node.facts.valid()
                    && short(&node.text_excerpt, 1024)
                    && node.states.connected
                    && node.attributes.len() <= ATTRIBUTES.len()
                    && node
                        .attributes
                        .iter()
                        .all(|(k, v)| ATTRIBUTES.contains(&k.as_str()) && short(v, 64))
                    && (!node.facts.sensitive || node.text_excerpt.is_empty())
            }
            Self::ReadAccessibility { .. } => false,
            Self::ReadValidation { validation, .. } => {
                query.kind == "read_validation"
                    && validation.reasons.len() <= 10
                    && validation.reasons.iter().collect::<BTreeSet<_>>().len()
                        == validation.reasons.len()
                    && (matches!(validation.state, ValidationState::Invalid)
                        != validation.reasons.is_empty())
            }
            Self::MatchRecord { matched, .. } => {
                query.kind == "match_record"
                    && query.node.is_some()
                    && query.anchor_text.is_some()
                    && query.expected_text.is_some()
                    && (!matched || !coverage.truncated)
            }
            Self::ReadLayout { layout, .. } => {
                let b = &layout.r#box;
                let v = &layout.viewport;
                query.kind == "read_layout"
                    && [b.x, b.y, b.width, b.height, v.width, v.height]
                        .into_iter()
                        .all(f64::is_finite)
                    && b.width >= 0.
                    && b.height >= 0.
                    && v.width >= 0.
                    && v.height >= 0.
                    && layout.computed.len() == STYLES.len()
                    && layout
                        .computed
                        .iter()
                        .all(|(k, v)| STYLES.contains(&k.as_str()) && short(v, 128))
                    && (layout.hit.kind == HitKind::Other) == layout.hit.node.is_some()
                    && layout.hit.node.is_none_or(serial)
            }
        };
        if !valid || !shape {
            return Err(types::decode());
        }
        Ok(raw)
    }
}

/// Retained identity has no URL, text, native object or action reference.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Target {
    pub context: Context,
    pub digest: [u8; 32],
    pub node: u64,
}
pub(crate) fn target(frames: &[Frame], frame: &Frame, node: u64) -> Result<Target> {
    let mut hash = Sha256::new();
    let mut current = Some(frame.context);
    let mut seen = BTreeSet::new();
    while let Some(context) = current {
        if !seen.insert(context) || seen.len() > types::MAX_FRAMES {
            return Err(types::stale());
        }
        let part = frames
            .iter()
            .find(|f| f.context == context)
            .ok_or_else(types::stale)?;
        // Hash the full identity, retaining only its digest in the public-ID table.
        hash.update(serde_json::to_vec(part).map_err(|_| types::decode())?);
        current = part.parent;
    }
    Ok(Target {
        context: frame.context,
        digest: hash.finalize().into(),
        node,
    })
}
fn ordered(mut frames: Vec<Frame>) -> Result<Vec<Frame>> {
    if frames.is_empty() || frames.len() > types::MAX_FRAMES {
        return Err(Failure::new("browser_frame_limit"));
    }
    frames.sort_by_key(|f| f.context);
    let roots = frames
        .iter()
        .filter(|f| f.parent.is_none())
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err(types::stale());
    }
    let mut ordered = vec![roots[0].clone()];
    let mut index = 0;
    while index < ordered.len() {
        let parent = ordered[index].context;
        for child in frames.iter().filter(|f| f.parent == Some(parent)) {
            if ordered.iter().any(|f| f.context == child.context) {
                return Err(types::stale());
            }
            ordered.push(child.clone());
        }
        index += 1;
    }
    if ordered.len() != frames.len() {
        return Err(types::stale());
    }
    Ok(ordered)
}
#[derive(Debug)]
pub(crate) struct Capture {
    pub frames: Vec<Frame>,
    pub records: Vec<(usize, Raw)>,
    pub coverage: Coverage,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
pub(crate) async fn capture<P: Port>(
    port: &P,
    query: &Query,
    binding: Option<Target>,
    deadline: Instant,
) -> Result<Capture> {
    let started_at_ms = now_ms();
    let frames = ordered(port.frames(deadline).await?)?;
    if let Some(binding) = binding {
        let frame = frames
            .iter()
            .find(|f| f.context == binding.context)
            .ok_or_else(types::stale)?;
        if target(&frames, frame, binding.node)?.digest != binding.digest {
            return Err(types::stale());
        }
    } else if query.id().is_some() {
        return Err(types::stale());
    }
    let selected = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| match query {
            Query::FindNodes {
                scope: Scope::MainDocument,
                ..
            } => f.parent.is_none(),
            Query::FindNodes { .. } => true,
            _ => binding.is_some_and(|b| b.context == f.context),
        })
        .collect::<Vec<_>>();
    let mut coverage = Coverage::base();
    let mut records = Vec::new();
    let mut remaining = match query {
        Query::FindNodes { limit, .. } => *limit,
        _ => 1,
    };
    for (index, frame) in selected {
        if remaining == 0 {
            coverage.limit(Limit::Result);
            break;
        }
        if coverage.visited_elements >= MAX_ELEMENTS {
            coverage.limit(Limit::Element);
            break;
        }
        if coverage.visited_text_nodes >= MAX_TEXT_NODES {
            coverage.limit(Limit::TextNode);
            break;
        }
        if coverage.visited_other_nodes >= MAX_OTHER_NODES {
            coverage.limit(Limit::OtherNode);
            break;
        }
        if &port.refresh(frame, deadline).await? != frame {
            return Err(types::stale());
        }
        let (tag, role, text) = match query {
            Query::FindNodes {
                tag, role, text, ..
            } => (tag.clone(), role.clone(), text.clone()),
            _ => (None, None, None),
        };
        let request = NativeQuery {
            kind: if query.native_ax() {
                "read_node"
            } else {
                query.kind()
            },
            tag,
            role,
            text,
            node: binding.map(|b| b.node),
            limit: remaining,
            elements: MAX_ELEMENTS - coverage.visited_elements,
            text_nodes: MAX_TEXT_NODES - coverage.visited_text_nodes,
            other_nodes: MAX_OTHER_NODES - coverage.visited_other_nodes,
            anchor_text: match query {
                Query::MatchRecord { anchor_text, .. } => Some(anchor_text.clone()),
                _ => None,
            },
            expected_text: match query {
                Query::MatchRecord { expected_text, .. } => Some(expected_text.clone()),
                _ => None,
            },
        };
        let Output::Evidence(mut raw) = port
            .program(
                frame,
                Program::Evidence {
                    marker: frame.marker.clone(),
                    query: request,
                },
                deadline,
            )
            .await?
        else {
            return Err(types::decode());
        };
        if query.native_ax() {
            let Raw::ReadNode { node, mut coverage } = raw else {
                return Err(types::decode());
            };
            let accessibility = port
                .native_accessibility(
                    frame,
                    binding.ok_or_else(types::stale)?.node,
                    &node.facts,
                    &mut coverage,
                    deadline,
                )
                .await?;
            raw = Raw::ReadAccessibility {
                accessibility,
                coverage,
            };
        }
        coverage.merge(raw.coverage());
        if let Raw::FindNodes { nodes, .. } = &raw {
            remaining -= nodes.len();
        }
        records.push((index, raw));
    }
    if ordered(port.frames(deadline).await?)? != frames {
        return Err(types::stale());
    }
    Ok(Capture {
        frames,
        records,
        coverage,
        started_at_ms,
        finished_at_ms: now_ms().max(started_at_ms),
    })
}
