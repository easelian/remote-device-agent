use super::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) const SOURCE: &str = include_str!("program.js");

pub(crate) async fn verify_reusable_submit<P: Port>(
    port: &P,
    frame: &Frame,
    radio: u64,
    submit: u64,
    deadline: Instant,
) -> Result<()> {
    let proof = Program::ReusableSubmit {
        marker: frame.marker.clone(),
        radio,
    };
    match port.program(frame, proof, deadline).await? {
        Output::ReusableSubmit(Some(id)) if id == submit => Ok(()),
        _ => Err(types::stale()),
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Program {
    Stamp,
    Evidence {
        marker: Marker,
        query: super::evidence::NativeQuery,
    },
    Surface,
    NativeFrameFocused,
    FrameAtPoint {
        x: f64,
        y: f64,
    },
    VisualRevision,
    VisualPath {
        x: f64,
        y: f64,
        to_x: f64,
        to_y: f64,
    },
    NativeFramePoint,
    ScrollViewport {
        delta: f64,
    },
    ScrollTarget {
        marker: Marker,
        node: u64,
        delta: f64,
    },
    Ready,
    Stats,
    ClearNodes {
        marker: Marker,
    },
    Open(Query),
    Next {
        marker: Marker,
        sequence: u64,
        nodes: usize,
        characters: usize,
    },
    Inspect {
        marker: Marker,
        node: u64,
    },
    ReusableSubmit {
        marker: Marker,
        radio: u64,
    },
    Geometry {
        marker: Marker,
        node: u64,
    },
    Recheck {
        marker: Marker,
        nodes: Vec<u64>,
        endpoint: bool,
    },
    Scroll {
        marker: Marker,
        node: u64,
    },
    Focus {
        marker: Marker,
        node: u64,
    },
    Form {
        marker: Marker,
        node: u64,
        value: String,
    },
    FormCheck {
        marker: Marker,
        node: u64,
        value: String,
    },
    SubmitProbeArm {
        marker: Marker,
        node: u64,
        enter: bool,
        register_field: bool,
    },
    SubmitProbeRead {
        marker: Marker,
        node: u64,
    },
    SubmitRecordArm {
        marker: Marker,
        submitter: u64,
        record: u64,
        anchor_text: String,
        expected_text: Option<String>,
    },
    SubmitRecordRead {
        marker: Marker,
        submitter: u64,
        record: u64,
    },
    SubmitFeedbackRead {
        marker: Marker,
        record: u64,
        anchor_text: String,
        fields: bool,
        include_indicators: bool,
    },
    SubmitPixelSlotsRead {
        marker: Marker,
        record: u64,
        anchor_text: String,
    },
    SubmitIconHoverArm {
        marker: Marker,
        record: u64,
        slot: PixelSlot,
        anchor_text: String,
    },
    SubmitPixelReasonRead {
        marker: Marker,
        record: u64,
        field: u64,
        source: u64,
        anchor_text: String,
    },
    Release,
}
pub(crate) const VISUAL_TARGET_NODE: &str = include_str!("visual-target.js");
pub(crate) const NATIVE_NODE: &str = "function(serialized){const a=JSON.parse(serialized),s=globalThis.__rdhSemanticDomV1;return s&&s.document===document&&s.realm===a.realm&&s.restoration===a.restoration?s.nodes.get(a.node)||null:null;}";
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitValidation {
    pub validation: super::evidence::Validation,
    pub field_node: Option<u64>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubmitFieldAssociation {
    Aria,
    AdjacentError,
    AdjacentText,
    SummaryError,
    SummaryText,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitField {
    pub field_node: u64,
    pub message_node: u64,
    pub reason: String,
    pub association: SubmitFieldAssociation,
}
impl SubmitFieldAssociation {
    pub(crate) fn is_strong(self) -> bool {
        matches!(self, Self::Aria | Self::AdjacentError | Self::SummaryError)
    }
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubmitVisualPart {
    BorderTop,
    BorderRight,
    BorderBottom,
    BorderLeft,
    Outline,
    Background,
}

fn nullable_color<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    serde::Deserialize::deserialize(d)
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitVisualChange {
    pub part: SubmitVisualPart,
    #[serde(deserialize_with = "nullable_color")]
    pub before: Option<String>,
    #[serde(deserialize_with = "nullable_color")]
    pub after: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitVisualField {
    pub field_node: u64,
    pub changes: Vec<SubmitVisualChange>,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubmitIndicatorKind {
    WrapperBorder,
    WrapperOutline,
    WrapperBackground,
    FieldShadow,
    WrapperShadow,
    Icon,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitIndicator {
    pub field_node: u64,
    pub source_node: u64,
    pub kind: SubmitIndicatorKind,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PixelKind {
    Reserved,
    ImageChange,
    PseudoChange,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) struct PixelSlot {
    pub field_node: u64,
    pub source_node: u64,
    pub kind: PixelKind,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub field_x: i32,
    pub field_y: i32,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub scroll_x: i32,
    pub scroll_y: i32,
    pub marked: bool,
    pub paint_revision: u32,
    pub changed: bool,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PixelSlotsReply {
    pub slots: Vec<PixelSlot>,
}
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PixelReasonSource {
    Title,
    AriaDescription,
    Tooltip,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PixelReason {
    pub source: PixelReasonSource,
    pub node: u64,
    pub text: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitFieldsReply {
    pub fields: Vec<SubmitField>,
    pub visual_changes: Vec<SubmitVisualField>,
    pub indicators: Option<Vec<SubmitIndicator>>,
}
impl SubmitFieldsReply {
    fn valid(&self, record: u64) -> bool {
        use std::collections::HashSet;
        if !valid_evidence_id(record) {
            return false;
        }
        if self.fields.len() > 16 || self.visual_changes.len() > 16 {
            return false;
        }
        if self.indicators.as_ref().is_some_and(|v| v.len() > 32) {
            return false;
        }
        let mut fields = HashSet::new();
        let mut messages = HashSet::new();
        for item in &self.fields {
            if !valid_page_reason(&item.reason)
                || !valid_evidence_id(item.field_node)
                || !valid_evidence_id(item.message_node)
                || item.field_node == record
                || item.message_node == record
                || !fields.insert(item.field_node)
                || !messages.insert(item.message_node)
            {
                return false;
            }
        }
        if !fields.is_disjoint(&messages) {
            return false;
        }
        let mut visual = HashSet::new();
        if let Some(indicators) = &self.indicators {
            let mut triples = HashSet::new();
            let mut owners = std::collections::HashMap::new();
            fields.extend(self.visual_changes.iter().map(|item| item.field_node));
            fields.extend(indicators.iter().map(|item| item.field_node));
            for item in indicators {
                let (field, source) = (item.field_node, item.source_node);
                if !valid_evidence_id(field)
                    || !valid_evidence_id(source)
                    || field == record
                    || source == record
                    || messages.contains(&field)
                    || messages.contains(&source)
                    || (item.kind == SubmitIndicatorKind::FieldShadow) != (source == field)
                    || (source != field && fields.contains(&source))
                    || owners
                        .insert(source, field)
                        .is_some_and(|owner| owner != field)
                    || !triples.insert((field, source, item.kind))
                {
                    return false;
                }
            }
        }
        self.visual_changes.iter().all(|item| {
            let mut parts = HashSet::new();
            valid_evidence_id(item.field_node)
                && !messages.contains(&item.field_node)
                && item.field_node != record
                && visual.insert(item.field_node)
                && (1..=6).contains(&item.changes.len())
                && item.changes.iter().all(|c| {
                    parts.insert(c.part)
                        && c.before != c.after
                        && [c.before.as_deref(), c.after.as_deref()]
                            .into_iter()
                            .flatten()
                            .all(|color| {
                                color.len() == 9
                                    && color.starts_with('#')
                                    && color[1..]
                                        .bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                            })
                })
        })
    }
}

fn valid_evidence_id(n: u64) -> bool {
    (1..=9_007_199_254_740_991).contains(&n)
}

fn valid_page_reason(text: &str) -> bool {
    !text.is_empty()
        && text.trim() == text
        && text.chars().count() <= 256
        && !text.chars().any(|c| {
            c.is_control()
                || matches!(u32::from(c),
            0x061c | 0x200b..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069 | 0xfeff)
        })
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubmitFeedbackKind {
    FieldError,
    RecordRejection,
    Alert,
    Status,
    RecordText,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeedbackNodes {
    pub kind: SubmitFeedbackKind,
    pub field_node: Option<u64>,
    pub message_node: u64,
    pub reason: String,
}
impl FeedbackNodes {
    fn valid(&self) -> bool {
        valid_evidence_id(self.message_node)
            && self
                .field_node
                .is_none_or(|n| valid_evidence_id(n) && n != self.message_node)
            && self.field_node.is_some() == (self.kind == SubmitFeedbackKind::FieldError)
            && valid_page_reason(&self.reason)
    }
}

pub(crate) enum Output {
    Evidence(super::evidence::Raw),
    Marker(Marker),
    Surface(super::surface::Metadata),
    Revision(u64),
    Batch(Batch),
    Facts(Option<Box<Facts>>),
    Checks(Vec<Option<Facts>>),
    Geometry(Option<Geometry>),
    Stats(RegistryStats),
    Applied(Option<bool>),
    ReusableSubmit(Option<u64>),
    Probe(Option<SubmitValidation>),
    SubmitEndpoint(Option<String>),
    Feedback(Option<FeedbackNodes>),
    Fields(SubmitFieldsReply),
    PixelSlots(Vec<PixelSlot>),
    PixelReason(Option<PixelReason>),
}
impl Program {
    pub fn source(&self) -> &'static str {
        if matches!(
            self,
            Self::Evidence { .. }
                | Self::SubmitProbeRead { .. }
                | Self::SubmitRecordArm { .. }
                | Self::SubmitRecordRead { .. }
                | Self::SubmitFeedbackRead { .. }
                | Self::SubmitPixelSlotsRead { .. }
                | Self::SubmitIconHoverArm { .. }
                | Self::SubmitPixelReasonRead { .. }
        ) {
            super::evidence::SOURCE
        } else {
            SOURCE
        }
    }
    pub fn effect(&self) -> crate::browser::control::Effect {
        use crate::browser::control::Effect;
        match self {
            Self::Scroll { .. }
            | Self::ScrollViewport { .. }
            | Self::ScrollTarget { .. }
            | Self::Focus { .. }
            | Self::Form { .. } => Effect::Page,
            _ => Effect::None,
        }
    }
    pub fn encode(&self) -> (&'static str, Value) {
        let target = |marker: &Marker, node| json!({"realm":marker.realm,"restoration":marker.restoration,"node":node});
        match self {
            Self::Evidence { marker, query } => {
                let mut args = serde_json::to_value(query).unwrap();
                args["realm"] = marker.realm.clone().into();
                args["restoration"] = marker.restoration.into();
                (query.kind, args)
            }
            Self::Stamp => ("stamp", json!({})),
            Self::Surface => ("surface", json!({})),
            Self::NativeFrameFocused => ("native_focused_frame", json!({})),
            Self::FrameAtPoint { x, y } => ("frame_at_point", json!({"x":x,"y":y})),
            Self::VisualRevision => ("visual_revision", json!({})),
            Self::VisualPath { x, y, to_x, to_y } => (
                "visual_path",
                json!({"x":x, "y":y, "to_x":to_x, "to_y":to_y}),
            ),
            Self::NativeFramePoint => ("native_frame_point", json!({})),
            Self::ScrollViewport { delta } => ("scroll_viewport", json!({"delta":delta})),
            Self::Ready => ("ready", json!({})),
            Self::Stats => ("stats", json!({})),
            Self::ClearNodes { marker } => (
                "clear_nodes",
                json!({"realm":marker.realm,"restoration":marker.restoration}),
            ),
            Self::Open(query) => ("open", serde_json::to_value(query).unwrap()),
            Self::Next {
                marker,
                sequence,
                nodes,
                characters,
            } => (
                "next",
                json!({"realm":marker.realm,"restoration":marker.restoration,"sequence":sequence,"nodes":nodes,"characters":characters}),
            ),
            Self::Inspect { marker, node } => ("inspect", target(marker, *node)),
            Self::ReusableSubmit { marker, radio } => ("reusable_submit", target(marker, *radio)),
            Self::Geometry { marker, node } => ("geometry", target(marker, *node)),
            Self::Recheck {
                marker,
                nodes,
                endpoint,
            } => (
                "recheck",
                json!({"realm":marker.realm,"restoration":marker.restoration,"nodes":nodes,"endpoint":endpoint}),
            ),
            Self::Scroll { marker, node } => ("scroll", target(marker, *node)),
            Self::ScrollTarget {
                marker,
                node,
                delta,
            } => {
                let mut args = target(marker, *node);
                args["delta"] = (*delta).into();
                ("scroll_target", args)
            }
            Self::Focus { marker, node } => ("focus", target(marker, *node)),
            Self::Form {
                marker,
                node,
                value,
            } => {
                let mut args = target(marker, *node);
                args["value"] = value.clone().into();
                ("form", args)
            }
            Self::FormCheck {
                marker,
                node,
                value,
            } => {
                let mut args = target(marker, *node);
                args["value"] = value.clone().into();
                ("form_check", args)
            }
            Self::SubmitProbeArm {
                marker,
                node,
                enter,
                register_field,
            } => {
                let mut args = target(marker, *node);
                args["enter"] = (*enter).into();
                args["register_field"] = (*register_field).into();
                ("submit_probe_arm", args)
            }
            Self::SubmitProbeRead { marker, node } => ("submit_probe_read", target(marker, *node)),
            Self::SubmitRecordArm {
                marker,
                submitter,
                record,
                anchor_text,
                expected_text,
            } => {
                let mut args = target(marker, *submitter);
                args["record"] = (*record).into();
                args["anchor_text"] = anchor_text.clone().into();
                args["expected_text"] = expected_text.clone().into();
                args["elements"] = 512.into();
                args["text_nodes"] = 256.into();
                args["other_nodes"] = 256.into();
                ("submit_record_arm", args)
            }
            Self::SubmitRecordRead {
                marker,
                submitter,
                record,
            } => {
                let mut args = target(marker, *submitter);
                args["record"] = (*record).into();
                ("submit_record_read", args)
            }
            Self::SubmitPixelSlotsRead {
                marker,
                record,
                anchor_text,
            } => (
                "submit_pixel_slots_read",
                json!({"realm":marker.realm,"restoration":marker.restoration,
                    "record":record,"anchor_text":anchor_text,
                    "elements":512,"text_nodes":256,"other_nodes":256}),
            ),
            Self::SubmitIconHoverArm {
                marker,
                record,
                slot,
                anchor_text,
            } => (
                "submit_pixel_hover_arm",
                json!({"realm":marker.realm,"restoration":marker.restoration,
                "record":record,"field":slot.field_node,"source":slot.source_node,"slot":slot,"anchor_text":anchor_text,
                "elements":512,"text_nodes":256,"other_nodes":256}),
            ),
            Self::SubmitPixelReasonRead {
                marker,
                record,
                field,
                source,
                anchor_text,
            } => (
                "submit_pixel_reason_read",
                json!({"realm":marker.realm,"restoration":marker.restoration,
                    "record":record,"field":field,"source":source,"anchor_text":anchor_text,
                    "elements":512,"text_nodes":256,"other_nodes":256}),
            ),
            Self::SubmitFeedbackRead {
                marker,
                record,
                anchor_text,
                fields,
                include_indicators,
            } => (
                if *fields {
                    "submit_fields_read"
                } else {
                    "submit_feedback_read"
                },
                json!({"realm":marker.realm,"restoration":marker.restoration,
                    "record":record,"anchor_text":anchor_text,"include_indicators":*include_indicators,
                    "elements":512,"text_nodes":256,"other_nodes":256}),
            ),
            Self::Release => ("release", json!({})),
        }
    }
    pub fn decode(&self, text: &str) -> Result<Output> {
        if text.len() > 1024 * 1024 {
            return Err(types::decode());
        }
        fn parse<T: DeserializeOwned>(text: &str) -> Result<T> {
            serde_json::from_str(text).map_err(|_| types::decode())
        }
        match self {
            Self::Evidence { query, .. } => {
                super::evidence::Raw::decode(text, query).map(Output::Evidence)
            }
            Self::Surface => {
                let value: super::surface::Metadata = parse(text)?;
                value.validate()?;
                Ok(Output::Surface(value))
            }
            Self::Stamp | Self::Open(_) => {
                let marker: Marker = parse(text)?;
                marker.validate()?;
                Ok(Output::Marker(marker))
            }
            Self::VisualRevision => Ok(Output::Revision(parse(text)?)),
            Self::Stats => {
                let stats: RegistryStats = parse(text)?;
                stats.validate()?;
                Ok(Output::Stats(stats))
            }
            Self::Next {
                nodes, characters, ..
            } => {
                if text == "null" {
                    return Err(types::stale());
                }
                let batch: Batch = parse(text)?;
                batch.validate(*nodes, *characters)?;
                Ok(Output::Batch(batch))
            }
            Self::ReusableSubmit { .. } => Ok(Output::ReusableSubmit(parse(text)?)),
            Self::Inspect { .. } => {
                let facts: Option<Box<Facts>> = parse(text)?;
                if let Some(facts) = &facts {
                    facts.validate()?;
                }
                Ok(Output::Facts(facts))
            }
            Self::Geometry { .. } | Self::NativeFramePoint => {
                let geometry: Option<Geometry> = parse(text)?;
                if let Some(value) = &geometry {
                    value.validate()?;
                }
                Ok(Output::Geometry(geometry))
            }
            Self::Recheck { nodes, .. } => {
                let facts: Vec<Option<Facts>> = parse(text)?;
                if facts.len() != nodes.len() || facts.len() > MAX_NODES {
                    return Err(types::decode());
                }
                for facts in facts.iter().flatten() {
                    facts.validate()?;
                }
                Ok(Output::Checks(facts))
            }
            Self::SubmitProbeRead { .. } => {
                let value: Option<SubmitValidation> = parse(text)?;
                Ok(Output::Probe(value))
            }
            Self::SubmitRecordArm { .. } => Ok(Output::SubmitEndpoint(parse(text)?)),
            Self::SubmitPixelSlotsRead { record, .. } => {
                let reply: PixelSlotsReply = parse(text)?;
                let mut ids = std::collections::HashSet::from([*record]);
                if reply.slots.len() > 16
                    || reply.slots.iter().any(|slot| {
                        !valid_evidence_id(slot.field_node)
                            || !valid_evidence_id(slot.source_node)
                            || !ids.insert(slot.field_node)
                            || !ids.insert(slot.source_node)
                            || !(6..=64).contains(&slot.width)
                            || !(6..=64).contains(&slot.height)
                            || !(1..=1920).contains(&slot.viewport_width)
                            || !(1..=1920).contains(&slot.viewport_height)
                            || slot
                                .x
                                .checked_add(slot.width)
                                .is_none_or(|x| x > slot.viewport_width)
                            || slot
                                .y
                                .checked_add(slot.height)
                                .is_none_or(|y| y > slot.viewport_height)
                    })
                {
                    return Err(types::decode());
                }
                Ok(Output::PixelSlots(reply.slots))
            }
            Self::SubmitIconHoverArm { .. } => Ok(Output::Applied(parse(text)?)),
            Self::SubmitPixelReasonRead { .. } => {
                let value: Option<PixelReason> = parse(text)?;
                if value
                    .as_ref()
                    .is_some_and(|r| !valid_evidence_id(r.node) || !valid_page_reason(&r.text))
                {
                    return Err(types::decode());
                }
                Ok(Output::PixelReason(value))
            }
            Self::SubmitFeedbackRead { fields: false, .. } => {
                let feedback: Option<FeedbackNodes> = parse(text)?;
                if feedback.as_ref().is_some_and(|item| !item.valid()) {
                    return Err(types::decode());
                }
                Ok(Output::Feedback(feedback))
            }
            Self::SubmitFeedbackRead {
                fields: true,
                include_indicators,
                record,
                ..
            } => {
                let reply: SubmitFieldsReply = parse(text)?;
                if !reply.valid(*record)
                    || (!include_indicators
                        && (reply.indicators.is_some()
                            || reply.fields.iter().any(|item| {
                                matches!(
                                    item.association,
                                    SubmitFieldAssociation::SummaryText
                                        | SubmitFieldAssociation::SummaryError
                                )
                            })))
                {
                    return Err(types::decode());
                }
                Ok(Output::Fields(reply))
            }
            _ => Ok(Output::Applied(parse(text)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields_program(include_indicators: bool) -> Program {
        Program::SubmitFeedbackRead {
            marker: Marker {
                realm: "r".into(),
                restoration: 0,
                url: "https://example.test/".into(),
            },
            record: 1,
            anchor_text: "Order B42".into(),
            fields: true,
            include_indicators,
        }
    }

    fn valid_reply() -> String {
        serde_json::json!({
            "fields": [{
                "field_node": 11,
                "message_node": 12,
                "reason": "Required",
                "association": "summary_error"
            }],
            "visual_changes": [],
            "indicators": [{
                "field_node": 11,
                "source_node": 11,
                "kind": "field_shadow"
            }]
        })
        .to_string()
    }

    #[test]
    fn submit_fields_requires_opt_in_for_indicators() {
        assert!(fields_program(true).decode(&valid_reply()).is_ok());
        assert!(fields_program(false).decode(&valid_reply()).is_err());
    }

    #[test]
    fn legacy_fields_decodes_only_original_associations() {
        let mut reply: serde_json::Value = serde_json::from_str(&valid_reply()).unwrap();
        reply.as_object_mut().unwrap().remove("indicators");
        assert!(fields_program(false).decode(&reply.to_string()).is_err());
        reply["fields"][0]["association"] = "adjacent_text".into();
        assert!(matches!(
            fields_program(false).decode(&reply.to_string()),
            Ok(Output::Fields(_))
        ));
    }

    #[test]
    fn submit_fields_accepts_summary_associations() {
        let mut value: serde_json::Value = serde_json::from_str(&valid_reply()).unwrap();
        value["fields"][0]["association"] = "summary_text".into();
        assert!(fields_program(true).decode(&value.to_string()).is_ok());
    }

    #[test]
    fn submit_fields_rejects_shared_indicator_source() {
        let mut value: serde_json::Value = serde_json::from_str(&valid_reply()).unwrap();
        value["indicators"] = serde_json::json!([
            {"field_node":11,"source_node":20,"kind":"wrapper_border"},
            {"field_node":13,"source_node":20,"kind":"wrapper_outline"}
        ]);
        assert!(fields_program(true).decode(&value.to_string()).is_err());
    }

    #[test]
    fn submit_fields_allows_multiple_indicator_kinds_for_one_field() {
        let mut value: serde_json::Value = serde_json::from_str(&valid_reply()).unwrap();
        value["indicators"] = serde_json::json!([
            {"field_node":11,"source_node":20,"kind":"wrapper_border"},
            {"field_node":11,"source_node":20,"kind":"wrapper_outline"}
        ]);
        assert!(fields_program(true).decode(&value.to_string()).is_ok());
    }

    #[test]
    fn submit_fields_rejects_record_collision() {
        let mut value: serde_json::Value = serde_json::from_str(&valid_reply()).unwrap();
        value["indicators"][0]["source_node"] = 1.into();
        assert!(fields_program(true).decode(&value.to_string()).is_err());
    }

    #[test]
    fn submit_fields_rejects_indicator_message_collision() {
        let mut value: serde_json::Value = serde_json::from_str(&valid_reply()).unwrap();
        value["indicators"][0]["source_node"] = 12.into();
        assert!(fields_program(true).decode(&value.to_string()).is_err());
    }

    #[test]
    fn legacy_feedback_rejects_fields_payload() {
        let program = Program::SubmitFeedbackRead {
            marker: Marker {
                realm: "r".into(),
                restoration: 0,
                url: "https://example.test/".into(),
            },
            record: 1,
            anchor_text: "Order B42".into(),
            fields: false,
            include_indicators: false,
        };
        assert!(program.decode(&valid_reply()).is_err());
    }
}
