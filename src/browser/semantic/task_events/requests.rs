//! Correlate request hops only within their diagnostic window. The same 32
//! slots own both the public rows and their private protocol bindings.
use super::FailureKind;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;

const LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Other,
}
impl Method {
    pub(crate) fn parse(raw: &str) -> Self {
        match raw {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            "OPTIONS" => Self::Options,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RequestUpdate {
    Start {
        method: Method,
        hop: Option<u32>,
        redirect: bool,
    },
    Response {
        status: u16,
        hop: Option<u32>,
    },
    Failed {
        reason: FailureKind,
        hop: Option<u32>,
    },
    Redirect {
        status: u16,
        hop: Option<u32>,
    },
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Phase {
    AwaitingResponse,
    Response {
        status: u16,
    },
    Redirected {
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
    },
    Failed {
        reason: FailureKind,
    },
}

struct Row {
    native: Box<str>,
    hop: Option<u32>,
    id: u64,
    method: Option<Method>,
    phase: Phase,
    from: Option<u64>,
    redirect: bool,
}

impl Row {
    fn public(&self, evicted: u64) -> Value {
        let mut value = json!(self.phase);
        value["request_id"] = self.id.into();
        value["start_observed"] = self.method.is_some().into();
        if let Some(method) = self.method {
            value["method"] = json!(method);
        }
        if self.redirect {
            if let Some(from) = self.from.filter(|&id| id > evicted) {
                value["redirected_from"] = from.into();
            } else {
                value["redirect_gap"] = true.into();
            }
        }
        value
    }
    fn redirected(&mut self) {
        let status = match self.phase {
            Phase::Response { status } if redirect_status(status) => Some(status),
            Phase::Redirected { status } => status,
            _ => None,
        };
        self.phase = Phase::Redirected { status };
    }
}
fn redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

#[derive(Default)]
pub(super) struct Ledger {
    rows: VecDeque<Row>,
    require_start: bool,
    evicted: u64,
    pub(super) untracked: u64,
}
impl Ledger {
    pub(super) fn for_epoch(epoch: u64) -> Self {
        // A completion first seen after control changed may belong to a human
        // or an earlier AI grant. Disclose only chains started in this window.
        Self {
            require_start: epoch != 1,
            ..Self::default()
        }
    }
    fn locate(&self, native: &str, hop: Option<u32>) -> Option<usize> {
        self.rows
            .iter()
            .rposition(|r| r.native.as_ref() == native && r.hop == hop)
    }
    fn append(&mut self, native: &str, hop: Option<u32>, redirect: bool) -> &mut Row {
        let from = if redirect {
            let previous = hop.and_then(|n| n.checked_sub(1));
            self.locate(native, previous).map(|index| {
                let row = &mut self.rows[index];
                row.redirected();
                row.id
            })
        } else {
            None
        };
        let id = self.evicted + self.rows.len() as u64 + 1;
        if self.rows.len() == LIMIT {
            self.rows.pop_front();
            self.evicted += 1;
        }
        self.rows.push_back(Row {
            native: native.into(),
            hop,
            id,
            method: None,
            phase: Phase::AwaitingResponse,
            from,
            redirect,
        });
        self.rows.back_mut().expect("just inserted a request")
    }
    pub(super) fn update(&mut self, native: &str, update: RequestUpdate) {
        if native.is_empty() || native.len() > 256 {
            self.untracked += 1;
            return;
        }
        let (hop, phase) = match update {
            RequestUpdate::Start {
                method,
                hop,
                redirect,
            } => {
                // BiDi names the hop explicitly; a repeated event is the same
                // hop. CDP's redirect reuses the previous hop's native ID.
                if self.require_start
                    && redirect
                    && self
                        .locate(native, hop.and_then(|n| n.checked_sub(1)))
                        .is_none()
                {
                    return;
                }
                let row = match hop.and_then(|n| self.locate(native, Some(n))) {
                    Some(i) => &mut self.rows[i],
                    None => self.append(native, hop, redirect),
                };
                row.method = Some(method);
                return;
            }
            RequestUpdate::Response { status, hop } if (100..=599).contains(&status) => {
                (hop, Phase::Response { status })
            }
            RequestUpdate::Failed { reason, hop } => (hop, Phase::Failed { reason }),
            RequestUpdate::Redirect { status, hop } if redirect_status(status) => (
                hop,
                Phase::Redirected {
                    status: Some(status),
                },
            ),
            _ => {
                self.untracked += 1;
                return;
            }
        };
        let row = match self.locate(native, hop) {
            Some(i) => &mut self.rows[i],
            None if self.require_start => return,
            None => self.append(native, hop, hop.is_some_and(|n| n > 0)),
        };
        match (row.phase, phase) {
            (Phase::Redirected { .. }, Phase::Response { status }) if redirect_status(status) => {
                row.phase = Phase::Redirected {
                    status: Some(status),
                }
            }
            (Phase::Failed { .. } | Phase::Redirected { .. }, Phase::Response { .. }) => {}
            _ => row.phase = phase,
        }
    }
    pub(super) fn snapshot(&self, state: &str) -> Value {
        json!({"state":state, "requests": self.rows.iter().map(|r|r.public(self.evicted)).collect::<Vec<_>>(),
            "evicted_count":self.evicted, "untracked_count":self.untracked})
    }
    pub(super) fn forget_bindings(&mut self) {
        for row in &mut self.rows {
            row.native = Box::default();
        }
    }
}

#[cfg(test)]
mod tests;
