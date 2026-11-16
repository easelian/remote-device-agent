use super::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) const SOURCE: &str = include_str!("program.js");

#[derive(Clone, Debug)]
pub(crate) enum Program {
    Stamp,
    Surface,
    NativeFrameFocused,
    FrameAtPoint {
        x: f64,
        y: f64,
    },
    NativeFramePoint,
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
    Release,
}
pub(crate) const NATIVE_NODE: &str = "function(serialized){const a=JSON.parse(serialized),s=globalThis.__rdhSemanticDomV1;return s&&s.document===document&&s.realm===a.realm&&s.restoration===a.restoration?s.nodes.get(a.node)||null:null;}";
pub(crate) enum Output {
    Marker(Marker),
    Surface(super::surface::Metadata),
    Batch(Batch),
    Facts(Option<Box<Facts>>),
    Checks(Vec<Option<Facts>>),
    Geometry(Option<Geometry>),
    Stats(RegistryStats),
    Applied(Option<bool>),
}
impl Program {
    pub fn effect(&self) -> crate::browser::control::Effect {
        use crate::browser::control::Effect;
        match self {
            Self::Scroll { .. } | Self::Focus { .. } | Self::Form { .. } => Effect::Page,
            _ => Effect::None,
        }
    }
    pub fn encode(&self) -> (&'static str, Value) {
        let target = |marker: &Marker, node| json!({"realm":marker.realm,"restoration":marker.restoration,"node":node});
        match self {
            Self::Stamp => ("stamp", json!({})),
            Self::Surface => ("surface", json!({})),
            Self::NativeFrameFocused => ("native_focused_frame", json!({})),
            Self::FrameAtPoint { x, y } => ("frame_at_point", json!({"x":x,"y":y})),
            Self::NativeFramePoint => ("native_frame_point", json!({})),
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
            Self::Recheck { marker, nodes } => (
                "recheck",
                json!({"realm":marker.realm,"restoration":marker.restoration,"nodes":nodes}),
            ),
            Self::Scroll { marker, node } => ("scroll", target(marker, *node)),
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
            _ => Ok(Output::Applied(parse(text)?)),
        }
    }
}
