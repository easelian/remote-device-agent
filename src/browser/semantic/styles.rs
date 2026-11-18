use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StyleProperty {
    Display,
    Visibility,
    PointerEvents,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StyleSnapshot {
    pub property: StyleProperty,
    pub candidates: Vec<StyleCandidate>,
    pub truncated: bool,
    cascade: Cascade,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Cascade {
    Undetermined,
}

impl StyleSnapshot {
    pub fn new(property: StyleProperty, candidates: Vec<StyleCandidate>, truncated: bool) -> Self {
        Self {
            property,
            candidates,
            truncated,
            cascade: Cascade::Undetermined,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StyleCandidate {
    pub origin: StyleOrigin,
    pub value: Option<String>,
    pub important: bool,
    pub sheet: Option<u8>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StyleOrigin {
    Inline,
    Author,
    UserAgent,
    Injected,
    Inspector,
}
