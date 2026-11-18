use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StyleScope {
    Direct,
    Ancestors,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestors_examined: Option<u8>,
    cascade: Cascade,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Cascade {
    Undetermined,
}

impl StyleSnapshot {
    pub fn new(property: StyleProperty, candidates: Vec<StyleCandidate>, truncated: bool) -> Self {
        Self::with_scope(property, candidates, truncated, None)
    }
    pub fn with_scope(
        property: StyleProperty,
        candidates: Vec<StyleCandidate>,
        truncated: bool,
        ancestors_examined: Option<u8>,
    ) -> Self {
        Self {
            property,
            candidates,
            truncated,
            ancestors_examined,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor_depth: Option<u8>,
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
