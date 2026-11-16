//! Browser surface facts, distinct from the canonical DOM observation.
use super::{Frame, Key, types};
use crate::browser::{Failure, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Dialog {
    pub dialog_id: String,
    pub kind: String,
    pub message: String,
}
impl Dialog {
    pub fn new(kind: &str, message: &str) -> Result<Self> {
        if !["alert", "confirm", "prompt", "beforeunload"].contains(&kind) {
            return Err(Failure::new("browser_dialog_unsupported"));
        }
        Ok(Self {
            dialog_id: crate::random::hex_token::<16>().map_err(crate::browser::io_error)?,
            kind: kind.into(),
            message: message.chars().take(4096).collect(),
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Tab {
    pub tab_id: String,
    pub url: String,
    pub title: String,
    pub ready: bool,
    pub dialog: Option<Dialog>,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Focus {
    pub node: u64,
    pub tag: String,
    pub input_type: String,
    pub value: String,
    pub start: Option<usize>,
    pub end: Option<usize>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Metadata {
    pub title: String,
    pub url: String,
    pub width: f64,
    pub height: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub focus: Option<Focus>,
}
impl Metadata {
    pub fn validate(&self) -> Result<()> {
        if self.title.len() > 16384
            || self.url.len() > 8192
            || [self.width, self.height, self.scroll_x, self.scroll_y]
                .iter()
                .any(|v| !v.is_finite())
            || !(1.0..=1920.0).contains(&self.width)
            || !(1.0..=1920.0).contains(&self.height)
            || self.focus.as_ref().is_some_and(|f| {
                f.node == 0 || f.value.len() > 65536 || f.tag.len() > 32 || f.input_type.len() > 32
            })
        {
            return Err(types::decode());
        }
        Ok(())
    }
    pub fn public(&self) -> serde_json::Value {
        serde_json::json!({"url":self.url,"title":self.title,"viewport":{"width":self.width,"height":self.height},"scroll_x":self.scroll_x,"scroll_y":self.scroll_y})
    }
}

/// Only the authenticated human endpoint constructs coordinates or raw text.
#[derive(Clone)]
pub(crate) enum HumanInput {
    Click {
        x: f64,
        y: f64,
    },
    Scroll {
        x: f64,
        y: f64,
        delta: f64,
    },
    Key(Key),
    Text {
        frame: Box<Frame>,
        focus: Focus,
        text: String,
    },
}
