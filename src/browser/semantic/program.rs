use super::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) const SOURCE: &str = include_str!("program.js");

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
        const MAX_JS_ID: u64 = 9_007_199_254_740_991;
        let id = |n: u64| (1..=MAX_JS_ID).contains(&n);
        let text = &self.reason;
        id(self.message_node)
            && self
                .field_node
                .is_none_or(|n| id(n) && n != self.message_node)
            && self.field_node.is_some() == (self.kind == SubmitFeedbackKind::FieldError)
            && !text.is_empty()
            && text.trim() == text
            && text.chars().count() <= 256
            && !text.chars().any(|c| {
                c.is_control()
                    || matches!(u32::from(c),
               0x061c | 0x200b..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069 | 0xfeff)
            })
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
    Probe(Option<SubmitValidation>),
    SubmitEndpoint(Option<String>),
    Feedback(Option<FeedbackNodes>),
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
            Self::SubmitFeedbackRead {
                marker,
                record,
                anchor_text,
            } => (
                "submit_feedback_read",
                json!({"realm":marker.realm,"restoration":marker.restoration,
                    "record":record,"anchor_text":anchor_text,"elements":512,"text_nodes":256,"other_nodes":256}),
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
            Self::SubmitFeedbackRead { .. } => {
                let feedback: Option<FeedbackNodes> = parse(text)?;
                if feedback.as_ref().is_some_and(|item| !item.valid()) {
                    return Err(types::decode());
                }
                Ok(Output::Feedback(feedback))
            }
            _ => Ok(Output::Applied(parse(text)?)),
        }
    }
}
