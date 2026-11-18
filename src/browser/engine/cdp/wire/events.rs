//! Shared event ownership covers both queued and consumer-held payloads.
use super::*;

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
    pub(super) fn capture_event(&self, value: &Value) -> Option<(Sink, Fact)> {
        let session = value["sessionId"].as_str()?;
        let task_events = self.task_events.lock().unwrap();
        let (root, sink) = task_events.as_ref()?;
        if session != root {
            return None;
        }
        if value["method"] == "Page.frameNavigated" {
            sink.invalidate();
            return None;
        }
        match value["method"].as_str()? {
            "Network.responseReceived" => value["params"]["response"]["status"]
                .as_u64()
                .and_then(|status| u16::try_from(status).ok())
                .map(|status| (sink.clone(), Fact::Response(status))),
            "Network.loadingFailed" => Some((
                sink.clone(),
                Fact::Failed(if value["params"]["canceled"].as_bool().unwrap_or(false) {
                    FailureKind::Canceled
                } else {
                    FailureKind::Other
                }),
            )),
            _ => None,
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
