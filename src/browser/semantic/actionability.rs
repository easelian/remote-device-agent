//! Typed preconditions shared by input admission and read-only diagnostics.
use super::{Facts, Geometry};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub(crate) const VERSION: u32 = 1;
pub(crate) const MAX_REASONS: usize = 8;
pub(crate) const MAX_BYTES: usize = 4096;
pub(crate) const TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) fn check_size(value: &serde_json::Value) -> crate::browser::Result<()> {
    if value["reasons"]
        .as_array()
        .is_none_or(|reasons| reasons.len() > MAX_REASONS)
        || serde_json::to_vec(value)
            .map_err(|_| super::types::decode())?
            .len()
            > MAX_BYTES
    {
        return Err(super::types::decode());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionKind {
    // Container scrolling has no public inspect kind.
    #[serde(skip)]
    Scroll,
    Hover,
    Click,
    Press,
    Fill,
    Check,
    Select,
    AttachFile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    ReadyNow,
    NeedsScroll,
    Blocked,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    ReadOnly,
    Covered,
    OutsideViewport,
    UnstableGeometry,
    UnsupportedGeometry,
    SensitiveTarget,
    UnsupportedAction,
}

/// These facts describe only the sampled prerequisites. No page value, native
/// identity, coordinates or text is part of the public diagnostic contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Conditions {
    pub read_only: bool,
    pub sensitive: bool,
    pub editable: bool,
    pub in_view: Option<bool>,
    pub hit: Option<bool>,
    pub stable: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Diagnostic {
    pub diagnostic_version: u32,
    pub action_kind: ActionKind,
    pub status: Status,
    pub reasons: Vec<Reason>,
    pub conditions: Conditions,
    pub content_trust: &'static str,
}

pub(crate) fn special_input(facts: &Facts) -> bool {
    matches!(
        facts.input_type.as_str(),
        "date" | "time" | "datetime-local" | "month" | "week" | "color" | "range"
    )
}

pub(crate) fn native_form(facts: &Facts) -> bool {
    facts.tag == "select" || facts.tag == "input" && special_input(facts)
}

/// Value/key validation, no-op detection and one-use upload authorization stay
/// in the actual action. The diagnostic has none of those request parameters.
pub(crate) fn requirements(facts: &Facts, kind: ActionKind, top_level: bool) -> Vec<Reason> {
    let writes = matches!(
        kind,
        ActionKind::Fill | ActionKind::Check | ActionKind::Select | ActionKind::AttachFile
    );
    let sensitive = facts.sensitive && matches!(kind, ActionKind::Fill | ActionKind::Select);
    let supported = (facts.kind != "scrollable" || kind == ActionKind::Scroll)
        && (facts.kind != "inferred"
            || matches!(
                kind,
                ActionKind::Click | ActionKind::Press | ActionKind::Hover
            ))
        && match kind {
            ActionKind::Scroll => facts.kind == "scrollable",
            ActionKind::Hover => matches!(facts.kind.as_str(), "native" | "aria" | "inferred"),
            ActionKind::Fill => facts.editable && !special_input(facts),
            ActionKind::Check => matches!(facts.role.as_str(), "checkbox" | "radio" | "switch"),
            ActionKind::Select => native_form(facts),
            ActionKind::AttachFile => {
                top_level
                    && facts.tag == "input"
                    && facts.input_type == "file"
                    && facts.kind == "native"
            }
            ActionKind::Click | ActionKind::Press => true,
        };
    let mut reasons = Vec::new();
    if writes && facts.readonly {
        reasons.push(Reason::ReadOnly);
    }
    if sensitive {
        reasons.push(Reason::SensitiveTarget);
    }
    if !supported {
        reasons.push(Reason::UnsupportedAction);
    }
    reasons
}

pub(crate) fn stable(first: (&Facts, &Geometry), second: (&Facts, &Geometry)) -> bool {
    first.0.x == second.0.x
        && first.0.y == second.0.y
        && first.1.x == second.1.x
        && first.1.y == second.1.y
}

/// Callers must validate reference/document identity and eligibility before
/// supplying either sample. None means geometry could not be proved, not a
/// missing target that may be resolved again by name or position.
pub(crate) fn assess(
    facts: &Facts,
    kind: ActionKind,
    top_level: bool,
    first: Option<&Geometry>,
    second: Option<(&Facts, &Geometry)>,
) -> Diagnostic {
    let current = second.or(first.map(|geometry| (facts, geometry)));
    let mut diagnostic = Diagnostic {
        diagnostic_version: VERSION,
        action_kind: kind,
        status: Status::Blocked,
        reasons: requirements(facts, kind, top_level),
        conditions: Conditions {
            read_only: facts.readonly,
            sensitive: facts.sensitive,
            editable: facts.editable,
            in_view: current.map(|(f, g)| f.in_view && g.in_view),
            hit: current.map(|(f, g)| f.hit && g.hit),
            stable: first
                .zip(second)
                .map(|(first, second)| stable((facts, first), second)),
        },
        content_trust: "untrusted_page",
    };
    if !diagnostic.reasons.is_empty() {
        return diagnostic;
    }
    let mut finish = |status, reason: Option<Reason>| {
        diagnostic.status = status;
        diagnostic.reasons = reason.into_iter().collect();
    };
    let Some(first) = first.filter(|g| g.visible) else {
        finish(Status::Indeterminate, Some(Reason::UnsupportedGeometry));
        return diagnostic;
    };
    if !facts.in_view || !first.in_view {
        finish(Status::NeedsScroll, Some(Reason::OutsideViewport));
    } else if !facts.hit || !first.hit {
        finish(Status::Blocked, Some(Reason::Covered));
    } else if let Some((next_facts, next_geometry)) = second {
        if !next_geometry.visible {
            finish(Status::Indeterminate, Some(Reason::UnsupportedGeometry));
        } else if !stable((facts, first), (next_facts, next_geometry)) {
            finish(Status::Indeterminate, Some(Reason::UnstableGeometry));
        } else if !next_facts.in_view || !next_geometry.in_view {
            finish(Status::NeedsScroll, Some(Reason::OutsideViewport));
        } else if !next_facts.hit || !next_geometry.hit {
            finish(Status::Blocked, Some(Reason::Covered));
        } else {
            finish(Status::ReadyNow, None);
        }
    } else {
        finish(Status::Indeterminate, Some(Reason::UnsupportedGeometry));
    }
    diagnostic
}
