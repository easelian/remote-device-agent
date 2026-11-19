//! Correlate request hops only within their diagnostic window. The same 32
//! slots own both the public rows and their private protocol bindings.
use super::FailureKind;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use url::Url;

const LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmitResponse {
    NotObserved,
    Pending,
    Undetermined,
    Response { request_id: u64, status: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RequestEndpoint([u8; 32]);
impl std::fmt::Debug for RequestEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RequestEndpoint").field(&"<digest>").finish()
    }
}
impl RequestEndpoint {
    pub(crate) fn from_url(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > 4096 || raw.bytes().any(|b| b.is_ascii_control()) {
            return None;
        }
        let mut url = Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        url.set_fragment(None);
        Some(Self(Sha256::digest(url.as_str().as_bytes()).into()))
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RequestUpdate {
    Start {
        method: Method,
        endpoint: Option<RequestEndpoint>,
        context: Option<Box<str>>,
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
    endpoint: Option<RequestEndpoint>,
    context: Option<Box<str>>,
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
    submit_after: u64,
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
    pub(super) fn for_action_epoch(epoch: u64) -> Self {
        // Action-owned captures must observe the request start themselves.
        Self {
            require_start: true,
            ..Self::for_epoch(epoch)
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
            endpoint: None,
            context: None,
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
                endpoint,
                context,
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
                row.endpoint = endpoint;
                row.context = context;
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
    pub(super) fn submit_dispatch(&mut self) {
        // Keep the public timeline and IDs stable. Only later starts can be
        // evidence for this input, even when an older response arrives later.
        self.submit_after = self.rows.back().map_or(self.evicted, |row| row.id);
    }

    pub(super) fn submit_response(
        &self,
        endpoint: &RequestEndpoint,
        method: Method,
        context: Option<&str>,
        active: bool,
    ) -> SubmitResponse {
        if !active || self.untracked > 0 || self.evicted > 0 {
            return SubmitResponse::Undetermined;
        }
        let mut matched = None;
        for row in self.rows.iter().filter(|row| row.id > self.submit_after) {
            if row.method.is_some_and(|observed| observed != method) {
                continue;
            }
            if row
                .endpoint
                .as_ref()
                .is_some_and(|observed| observed != endpoint)
            {
                continue;
            }
            if row.context.is_some() && row.context.as_deref() != context {
                continue;
            }
            if row.method.is_none()
                || row.endpoint.is_none()
                || row.context.as_deref() != context
                || row.redirect
                || matched.is_some()
            {
                return SubmitResponse::Undetermined;
            }
            matched = Some(row);
        }
        match matched {
            None => SubmitResponse::NotObserved,
            Some(row) => match row.phase {
                Phase::AwaitingResponse => SubmitResponse::Pending,
                Phase::Response { status } => SubmitResponse::Response {
                    request_id: row.id,
                    status,
                },
                _ => SubmitResponse::Undetermined,
            },
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
