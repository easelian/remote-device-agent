//! Native accessibility facts, separate from the canonical DOM profile.
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Present,
    Absent,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NameStatus {
    Available,
    Omitted,
    Absent,
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub status: Status,
    pub role: Option<String>,
    pub name: Option<String>,
    pub name_status: NameStatus,
    pub ignored: Option<bool>,
    pub states: BTreeMap<String, Value>,
    pub ignored_reasons: BTreeSet<String>,
}
impl Snapshot {
    pub fn absent() -> Self {
        Self {
            status: Status::Absent,
            role: None,
            name: None,
            name_status: NameStatus::Absent,
            ignored: None,
            states: BTreeMap::new(),
            ignored_reasons: BTreeSet::new(),
        }
    }
}
