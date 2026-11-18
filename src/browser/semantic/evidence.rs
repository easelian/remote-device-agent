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
#[derive(Clone, Debug)]
pub(crate) enum Query {
    FindNodes {
        tag: Option<String>,
        role: Option<String>,
        text: Option<String>,
        scope: Scope,
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
    ReadStyles {
        node_id: String,
        property: super::styles::StyleProperty,
    },
    ReadAncestorStyles {
        node_id: String,
        property: super::styles::StyleProperty,
    },
    // Internal-only query used by record_wait after resolving an evidence binding.
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
        if value["kind"] == "find_nodes" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct FindRequest<'a> {
                #[serde(rename = "kind")]
                _kind: serde::de::IgnoredAny,
                tag: Option<&'a str>,
                role: Option<&'a str>,
                text: Option<&'a str>,
                #[serde(default)]
                scope: Scope,
                #[serde(default = "default_limit")]
                limit: usize,
            }
            let find = FindRequest::deserialize(value).map_err(|_| invalid())?;
            if find.tag.is_none() && find.role.is_none() && find.text.is_none()
                || find.tag.is_some_and(|tag| !identifier(tag))
                || find.role.is_some_and(|role| !identifier(role))
                || find
                    .text
                    .is_some_and(|text| text.is_empty() || text.chars().count() > 128)
                || !(1..=MAX_RESULTS).contains(&find.limit)
            {
                return Err(invalid());
            }
            return Ok(Self::FindNodes {
                tag: find.tag.map(str::to_owned),
                role: find.role.map(str::to_owned),
                text: find.text.map(str::to_owned),
                scope: find.scope,
                limit: find.limit,
            });
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NodeRequest<'a> {
            kind: &'a str,
            node_id: &'a str,
            property: Option<super::styles::StyleProperty>,
        }
        let target = NodeRequest::deserialize(value).map_err(|_| invalid())?;
        if !node_id(target.node_id) {
            return Err(invalid());
        }
        let node_id = target.node_id.to_owned();
        match (target.kind, target.property) {
            ("read_node", None) => Ok(Self::ReadNode { node_id }),
            ("read_layout", None) => Ok(Self::ReadLayout { node_id }),
            ("read_accessibility", None) => Ok(Self::ReadAccessibility { node_id }),
            ("read_validation", None) => Ok(Self::ReadValidation { node_id }),
            ("read_styles", Some(property)) => Ok(Self::ReadStyles { node_id, property }),
            ("read_ancestor_styles", Some(property))
                if property != super::styles::StyleProperty::Display =>
            {
                Ok(Self::ReadAncestorStyles { node_id, property })
            }
            _ => Err(invalid()),
        }
    }
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::FindNodes { .. } => None,
            Self::ReadNode { node_id }
            | Self::ReadLayout { node_id }
            | Self::ReadAccessibility { node_id }
            | Self::ReadValidation { node_id }
            | Self::ReadStyles { node_id, .. }
            | Self::ReadAncestorStyles { node_id, .. }
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
            Self::ReadStyles { .. } => "read_styles",
            Self::ReadAncestorStyles { .. } => "read_ancestor_styles",
            Self::MatchRecord { .. } => "match_record",
        }
    }
    pub fn native_ax(&self) -> bool {
        matches!(self, Self::ReadAccessibility { .. })
    }
    pub fn native_css(&self) -> bool {
        matches!(
            self,
            Self::ReadStyles { .. } | Self::ReadAncestorStyles { .. }
        )
    }
    pub fn style_scope(&self) -> super::styles::StyleScope {
        match self {
            Self::ReadAncestorStyles { .. } => super::styles::StyleScope::Ancestors,
            _ => super::styles::StyleScope::Direct,
        }
    }
    pub fn source(&self) -> &'static str {
        if self.native_ax() {
            "native_ax"
        } else if self.native_css() {
            "native_css"
        } else {
            "dom"
        }
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
    #[serde(rename = "sensitive_style_rules")]
    SensitiveStyleRules,
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
/// DOM replies are decoded here. Native adapters construct the AX and CSS variants directly.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    ReadStyles {
        styles: super::styles::StyleSnapshot,
        coverage: Coverage,
    },
    MatchRecord {
        matched: bool,
        coverage: Coverage,
    },
    StyleTarget {
        sensitive: bool,
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
            | Self::ReadStyles { coverage, .. }
            | Self::MatchRecord { coverage, .. }
            | Self::StyleTarget { coverage, .. } => coverage,
        }
    }
    pub fn decode(text: &str, query: &NativeQuery) -> Result<Self> {
        if text.len() > MAX_BYTES {
            return Err(types::decode());
        }
        // A missing DOM node is represented by JSON null regardless of the query.
        if text.trim_matches([' ', '\n', '\r', '\t']) == "null" {
            return Err(types::stale());
        }
        // The operation is known before reading the response. Decode its closed
        // shape directly so Serde need not buffer a tagged value and decode it again.
        macro_rules! dom_reply {
            ($variant:ident, $field:ident: $ty:ty, $tag:expr) => {{
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Reply<'a> {
                    #[serde(borrow)]
                    kind: std::borrow::Cow<'a, str>,
                    $field: $ty,
                    coverage: Coverage,
                }
                let reply: Reply = serde_json::from_str(text).map_err(|_| types::decode())?;
                if reply.kind != $tag {
                    return Err(types::decode());
                }
                Self::$variant {
                    $field: reply.$field,
                    coverage: reply.coverage,
                }
            }};
        }
        let raw = match query.kind {
            "find_nodes" => dom_reply!(FindNodes, nodes: Vec<Node>, query.kind),
            "read_node" => dom_reply!(ReadNode, node: Detail, query.kind),
            "read_layout" => dom_reply!(ReadLayout, layout: Layout, query.kind),
            "read_validation" => dom_reply!(ReadValidation, validation: Validation, query.kind),
            "match_record" => dom_reply!(MatchRecord, matched: bool, query.kind),
            "style_target" | "ancestor_style_target" => {
                dom_reply!(StyleTarget, sensitive: bool, "style_target")
            }
            _ => return Err(types::decode()),
        };
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
                nodes.len() <= query.limit
                    && nodes.iter().all(Node::valid)
                    && nodes.iter().map(|n| n.node).collect::<BTreeSet<_>>().len() == nodes.len()
            }
            Self::ReadNode { node, .. } => {
                Some(node.facts.node) == query.node
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
            Self::ReadValidation { validation, .. } => {
                validation.reasons.len() <= 10
                    && validation.reasons.iter().collect::<BTreeSet<_>>().len()
                        == validation.reasons.len()
                    && (matches!(validation.state, ValidationState::Invalid)
                        != validation.reasons.is_empty())
            }
            Self::ReadAccessibility { .. } | Self::ReadStyles { .. } => false,
            Self::StyleTarget { .. } => query.node.is_some(),
            Self::MatchRecord { matched, .. } => {
                query.node.is_some()
                    && query.anchor_text.is_some()
                    && query.expected_text.is_some()
                    && (!matched || !coverage.truncated)
            }
            Self::ReadLayout { layout, .. } => {
                let b = &layout.r#box;
                let v = &layout.viewport;
                [b.x, b.y, b.width, b.height, v.width, v.height]
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
async fn native_facts<P: Port>(
    port: &P,
    frame: &Frame,
    query: NativeQuery,
    deadline: Instant,
) -> Result<Raw> {
    match port
        .program(
            frame,
            Program::Evidence {
                marker: frame.marker.clone(),
                query,
            },
            deadline,
        )
        .await?
    {
        Output::Evidence(raw) => Ok(raw),
        _ => Err(types::decode()),
    }
}

async fn capture_styles<P: Port>(
    port: &P,
    frame: &Frame,
    request: NativeQuery,
    property: super::styles::StyleProperty,
    scope: super::styles::StyleScope,
    deadline: Instant,
) -> Result<Raw> {
    let node = request.node.ok_or_else(types::stale)?;
    let Raw::StyleTarget {
        sensitive,
        mut coverage,
    } = native_facts(port, frame, request.clone(), deadline).await?
    else {
        return Err(types::decode());
    };
    if !sensitive {
        let snapshot = port
            .native_styles(frame, node, property, scope, deadline)
            .await?;
        let Raw::StyleTarget {
            sensitive,
            coverage: after,
        } = native_facts(port, frame, request, deadline).await?
        else {
            return Err(types::decode());
        };
        coverage.merge(&after);
        if !sensitive {
            return Ok(Raw::ReadStyles {
                styles: snapshot,
                coverage,
            });
        }
    }
    coverage.omitted.insert(Omitted::SensitiveStyleRules);
    Ok(Raw::ReadStyles {
        styles: match scope {
            super::styles::StyleScope::Ancestors => {
                super::styles::StyleSnapshot::with_scope(property, Vec::new(), false, Some(0))
            }
            super::styles::StyleScope::Direct => {
                super::styles::StyleSnapshot::new(property, Vec::new(), false)
            }
        },
        coverage,
    })
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
            } else if query.native_css() {
                if query.style_scope() == super::styles::StyleScope::Ancestors {
                    "ancestor_style_target"
                } else {
                    "style_target"
                }
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
        let mut raw = if let Query::ReadStyles { property, .. }
        | Query::ReadAncestorStyles { property, .. } = query
        {
            capture_styles(
                port,
                frame,
                request,
                *property,
                query.style_scope(),
                deadline,
            )
            .await?
        } else {
            native_facts(port, frame, request, deadline).await?
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
