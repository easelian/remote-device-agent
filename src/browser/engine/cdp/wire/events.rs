//! Shared event ownership covers both queued and consumer-held payloads.
use super::*;
#[cfg(feature = "experimental-browser-bidi")]
use crate::browser::semantic::task_events::{Method, RequestEndpoint, RequestUpdate};

pub(super) const MAX_EVENTS: usize = 256;
pub(super) const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONSUMERS: usize = 2;

pub(super) struct Envelope {
    value: Value,
    sequence: u64,
    _count: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

/// Clones share the payload and its permits; decoding never releases them.
#[derive(Clone)]
pub(in crate::browser::engine) struct EventLease(Arc<Envelope>);
impl std::ops::Deref for EventLease {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0.value
    }
}

pub(in crate::browser) struct EventReceiver {
    raw: broadcast::Receiver<Arc<Envelope>>,
    owner: Arc<Inner>,
    stopped: watch::Receiver<Shutdown>,
    next: u64,
    registered: bool,
}
impl EventReceiver {
    pub async fn recv(&mut self) -> Result<EventLease> {
        if self.owner.closed.load(Ordering::Acquire) || self.owner.fenced.load(Ordering::Acquire) {
            return Err(Failure::unknown("browser_lost"));
        }
        let result = tokio::select! {
            biased;
            _ = self.stopped.changed() => None,
            value = self.raw.recv() => value.ok(),
        };
        if let Some(event) = result
            && event.sequence == self.next
            && !self.owner.fenced.load(Ordering::Acquire)
            && !self.owner.closed.load(Ordering::Acquire)
        {
            self.next += 1;
            return Ok(EventLease(event));
        }
        self.owner
            .fence_events(&mut self.owner.pending.lock().unwrap());
        Err(Failure::unknown("browser_lost"))
    }
}
impl Drop for EventReceiver {
    fn drop(&mut self) {
        if self.registered {
            let mut pending = self.owner.pending.lock().unwrap();
            pending.event_receivers -= 1;
            // Every registered consumer is essential. Another subscriber must
            // not hide an exited navigation or lifecycle owner.
            self.owner.fence_events(&mut pending);
        }
    }
}

impl Cdp {
    pub fn events(&self) -> EventReceiver {
        let mut pending = self.inner.pending.lock().unwrap();
        let registered = self.connected() && pending.event_receivers < MAX_CONSUMERS;
        if registered {
            pending.event_receivers += 1;
        } else {
            self.inner.fence_events(&mut pending);
        }
        EventReceiver {
            raw: self.events.subscribe(),
            owner: self.inner.clone(),
            stopped: self.inner.stop.subscribe(),
            next: pending.event_sequence + 1,
            registered,
        }
    }

    pub fn require_event_consumer(&self) -> Result<()> {
        let mut pending = self.inner.pending.lock().unwrap();
        if self.connected() && pending.event_receivers > 0 {
            Ok(())
        } else {
            self.inner.fence_events(&mut pending);
            Err(Failure::unknown("browser_lost"))
        }
    }
}

#[cfg(feature = "experimental-browser-bidi")]
pub(super) struct Captured {
    pub(super) sink: Sink,
    pub(super) fact: Option<Fact>,
    pub(super) progress: Vec<(String, RequestUpdate)>,
    pub(super) journal: Option<crate::browser::semantic::task_events::journal::Net>,
}

impl Inner {
    pub(super) fn fence_events(&self, pending: &mut PendingState) {
        if self.closed.load(Ordering::Acquire) || self.fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        // This mutex is also the command admission boundary. Notify callers,
        // but retain reservations and native ownership until replies or EOF.
        pending.event_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(3));
        for command in pending.commands.values_mut() {
            if let Some(reply) = command.reply.take() {
                let _ = reply.send(Err(Failure::unknown("browser_lost")));
            }
        }
        self.stop.send_replace(Shutdown::Fenced);
    }

    #[cfg(feature = "experimental-browser-bidi")]
    pub(super) fn capture_event(&self, value: &Value) -> Option<Captured> {
        let session = value["sessionId"].as_str()?;
        let task_events = self.task_events.lock().unwrap();
        let (root, sink) = task_events.as_ref()?;
        if session != root {
            return None;
        }
        let method = value["method"].as_str()?;
        if sink.journaling() {
            let journal = crate::browser::semantic::task_events::journal::Net::parse(
                method,
                &value["params"],
            )?;
            return Some(Captured {
                sink: sink.clone(),
                fact: None,
                progress: Vec::new(),
                journal: Some(journal),
            });
        }
        if method == "Page.frameNavigated" {
            sink.invalidate();
            return None;
        }
        let fact = match method {
            "Network.responseReceived" if sink.wants_network() => {
                value["params"]["response"]["status"]
                    .as_u64()
                    .and_then(|status| u16::try_from(status).ok())
                    .map(|status| Fact::Response(status))
            }
            "Network.loadingFailed" if sink.wants_network() => Some(Fact::Failed(
                if value["params"]["canceled"].as_bool().unwrap_or(false) {
                    FailureKind::Canceled
                } else {
                    FailureKind::Other
                },
            )),
            _ => None,
        };
        let progress = if sink.wants_progress() {
            Self::progress_update(method, &value["params"], sink.tracks_submit())
        } else {
            Vec::new()
        };
        if fact.is_none() && progress.is_empty() {
            return None;
        }
        Some(Captured {
            sink: sink.clone(),
            fact,
            progress,
            journal: None,
        })
    }

    #[cfg(feature = "experimental-browser-bidi")]
    fn progress_update(
        method: &str,
        params: &Value,
        track_submit: bool,
    ) -> Vec<(String, RequestUpdate)> {
        let request_id = || {
            params["requestId"]
                .as_str()
                .filter(|id| id.len() <= 256)
                .map(str::to_owned)
                .unwrap_or_default()
        };
        match method {
            "Network.requestWillBeSent" => {
                let request_id = request_id();
                let method = Method::parse(
                    params
                        .get("request")
                        .and_then(|request| request["method"].as_str())
                        .unwrap_or(""),
                );
                let redirect = params.get("redirectResponse").is_some();
                let mut update = Vec::new();
                if let Some(status) = params["redirectResponse"]["status"]
                    .as_u64()
                    .and_then(|status| u16::try_from(status).ok())
                {
                    update.push((
                        request_id.clone(),
                        RequestUpdate::Redirect { status, hop: None },
                    ));
                }
                update.push((
                    request_id,
                    RequestUpdate::Start {
                        method,
                        endpoint: params["request"]["url"]
                            .as_str()
                            .filter(|_| track_submit)
                            .and_then(RequestEndpoint::from_url),
                        context: params["frameId"]
                            .as_str()
                            .filter(|id| track_submit && !id.is_empty() && id.len() <= 256)
                            .map(Into::into),
                        hop: None,
                        redirect,
                    },
                ));
                // A single CDP delivery contains both the redirect settlement
                // and the next start. The caller emits them in order.
                update
            }
            "Network.responseReceived" => vec![(
                request_id(),
                RequestUpdate::Response {
                    status: params["response"]["status"]
                        .as_u64()
                        .and_then(|n| u16::try_from(n).ok())
                        .unwrap_or(0),
                    hop: None,
                },
            )],
            "Network.loadingFailed" => vec![(
                request_id(),
                RequestUpdate::Failed {
                    reason: Self::failure_reason(params),
                    hop: None,
                },
            )],
            _ => Vec::new(),
        }
    }

    #[cfg(feature = "experimental-browser-bidi")]
    fn failure_reason(params: &Value) -> FailureKind {
        if params["canceled"].as_bool().unwrap_or(false) {
            return FailureKind::Canceled;
        }
        let text = params["errorText"]
            .as_str()
            .filter(|s| s.len() <= 512)
            .unwrap_or("");
        if text.contains("NAME_NOT_RESOLVED") {
            FailureKind::Dns
        } else if text.contains("TIMED_OUT") || text.contains("TIMEOUT") {
            FailureKind::Timeout
        } else if text.contains("CONNECTION") {
            FailureKind::Connection
        } else {
            FailureKind::Other
        }
    }

    pub(super) fn publish_event(
        &self,
        pending: &mut PendingState,
        sender: &broadcast::Sender<Arc<Envelope>>,
        value: Value,
        encoded_bytes: usize,
    ) {
        if self.closed.load(Ordering::Acquire) || self.fenced.load(Ordering::Acquire) {
            return;
        }
        if !matches!(
            value["method"].as_str(),
            Some(
                "Target.attachedToTarget"
                    | "Target.detachedFromTarget"
                    | "Target.targetInfoChanged"
                    | "Fetch.requestPaused"
                    | "Network.responseReceivedExtraInfo"
                    | "Page.javascriptDialogOpening"
                    | "Page.javascriptDialogClosed"
            )
        ) {
            return;
        }

        let permits = self
            .event_slots
            .clone()
            .try_acquire_owned()
            .ok()
            .and_then(|count| {
                self.event_bytes
                    .clone()
                    .try_acquire_many_owned(encoded_bytes as u32)
                    .ok()
                    .map(|bytes| (count, bytes))
            });
        let Some((count, bytes)) = permits.filter(|_| pending.event_receivers > 0) else {
            self.fence_events(pending);
            return;
        };
        #[cfg(test)]
        {
            pending.event_peak.0 = pending
                .event_peak
                .0
                .max(MAX_EVENTS - self.event_slots.available_permits());
            pending.event_peak.1 = pending
                .event_peak
                .1
                .max(MAX_EVENT_BYTES - self.event_bytes.available_permits());
        }
        if let Some(session) = value["sessionId"].as_str() {
            match value["method"].as_str() {
                Some("Page.javascriptDialogOpening") => {
                    pending.dialogs.insert(session.into());
                    for command in pending
                        .commands
                        .values_mut()
                        .filter(|c| c.session.as_deref() == Some(session) && c.dialog_blocks)
                    {
                        if let Some(reply) = command.reply.take() {
                            let _ = reply.send(Err(Failure::unknown("browser_dialog_open")));
                        }
                    }
                }
                Some("Page.javascriptDialogClosed") => {
                    pending.dialogs.remove(session);
                }
                _ => {}
            }
        }
        pending.event_sequence += 1;
        let event = Arc::new(Envelope {
            value,
            sequence: pending.event_sequence,
            _count: count,
            _bytes: bytes,
        });
        if sender.send(event).is_err() {
            self.fence_events(pending);
        }
    }
}

#[cfg(all(test, feature = "experimental-browser-bidi"))]
mod progress_tests {
    use super::*;
    use crate::browser::semantic::task_events::{FailureKind, RequestUpdate};

    #[test]
    fn redirect_settles_before_new_start_without_payload() {
        let updates = Inner::progress_update(
            "Network.requestWillBeSent",
            &json!({
                "requestId":"req-1",
                "request":{"method":"GET","url":"https://secret.invalid/private"},
                "redirectResponse":{"status":302,"url":"https://secret.invalid/old"}
            }),
            false,
        );
        assert_eq!(updates.len(), 2);
        assert!(matches!(
            updates[0].1,
            RequestUpdate::Redirect { status: 302, .. }
        ));
        assert!(matches!(
            updates[1].1,
            RequestUpdate::Start { redirect: true, .. }
        ));
    }

    #[test]
    fn response_and_failure_are_typed_and_bounded() {
        let response = Inner::progress_update(
            "Network.responseReceived",
            &json!({"requestId":"r","response":{"status":204,"url":"secret"}}),
            false,
        );
        assert!(matches!(
            response.as_slice(),
            [(id, RequestUpdate::Response { status: 204, .. })] if id == "r"
        ));
        let failed = Inner::progress_update(
            "Network.loadingFailed",
            &json!({"requestId":"r","canceled":false,"errorText":"net::ERR_NAME_NOT_RESOLVED","url":"secret"}),
            false,
        );
        assert!(matches!(
            failed.as_slice(),
            [(
                _,
                RequestUpdate::Failed {
                    reason: FailureKind::Dns,
                    ..
                }
            )]
        ));
        let missing = Inner::progress_update("Network.requestWillBeSent", &json!({}), false);
        assert_eq!(missing.len(), 1);
        assert!(matches!(
            missing.as_slice(),
            [(id, RequestUpdate::Start { method: Method::Other, .. })] if id.is_empty()
        ));
        let oversized = Inner::progress_update(
            "Network.responseReceived",
            &json!({"requestId":"x".repeat(257),"response":{"status":200}}),
            false,
        );
        assert!(matches!(
            oversized.as_slice(),
            [(id, RequestUpdate::Response { status: 200, .. })] if id.is_empty()
        ));
        let failed = Inner::progress_update(
            "Network.loadingFailed",
            &json!({"requestId":"r","errorText":"net::ERR_FAILED"}),
            false,
        );
        assert!(matches!(
            failed.as_slice(),
            [(
                _,
                RequestUpdate::Failed {
                    reason: FailureKind::Other,
                    ..
                }
            )]
        ));
    }
}
