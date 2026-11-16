//! One pending owner per command, independent read/write tasks and a bounded
//! critical-event path. Caller cancellation never cancels an accepted command.
use super::{Command, ContextId, Event, event::CORE_EVENTS};
use crate::browser::{
    Failure,
    control::{Control, Effect, Submission},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[cfg(test)]
mod tests;

pub(super) const MAX_MESSAGE: usize = 2 * 1024 * 1024;
const MAX_OUTGOING: usize = 256 * 1024;
const MAX_EVENT: usize = 64 * 1024;
const EVENT_BYTES: usize = 1024 * 1024;
const EVENT_COUNT: usize = 256;
const SLOTS: usize = 32;
const MAX_ID: u64 = (1 << 53) - 1;
static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);

pub(super) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_OUTGOING + 1024)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Fault {
    Disconnected,
    Malformed,
    MessageLimit,
    EventGap,
    WriteTimeout,
    Closed,
    Startup,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProtocolError {
    InvalidArgument,
    InvalidSession,
    NoSuchFrame,
    UnknownCommand,
    Unsupported,
    Other,
}
impl ProtocolError {
    fn parse(text: &str) -> Self {
        match text {
            "invalid argument" => Self::InvalidArgument,
            "invalid session id" => Self::InvalidSession,
            "no such frame" => Self::NoSuchFrame,
            "unknown command" => Self::UnknownCommand,
            "unsupported operation" => Self::Unsupported,
            _ => Self::Other,
        }
    }
}
#[derive(Debug)]
pub(super) enum Error {
    Rejected(Failure),
    Transport(Fault),
    Protocol(ProtocolError),
    TimeoutUnknown,
    DeliveryRevoked,
    CleanupPending,
}
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Settlement {
    Enqueued,
    Success,
    ProtocolError(ProtocolError),
    Unknown(Fault),
}
#[derive(Clone)]
pub(super) struct Receipt {
    id: u64,
    status: Arc<Mutex<Settlement>>,
}
impl Receipt {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn status(&self) -> Settlement {
        *self.status.lock().unwrap()
    }
    fn settle(&self, value: Settlement) {
        *self.status.lock().unwrap() = value;
    }
}
#[derive(Debug)]
pub(super) struct Reply {
    pub value: Value,
    pub event_boundary: u64,
}
pub(super) struct Call {
    pub receipt: Receipt,
    result: oneshot::Receiver<Result<Reply>>,
    submission: Submission,
    deadline: Instant,
}
impl Call {
    pub async fn wait(self) -> Result<Reply> {
        let result = match tokio::time::timeout_at(self.deadline.into(), self.result).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => Err(Error::Transport(Fault::Disconnected)),
            Err(_) => Err(Error::TimeoutUnknown),
        };
        // Rejected delivery cannot erase the already settled native receipt or
        // expose its content to the old control epoch.
        self.submission
            .deliver(result)
            .map_err(|_| Error::DeliveryRevoked)?
    }
}

struct Pending {
    reply: oneshot::Sender<Result<Reply>>,
    receipt: Receipt,
    effect: Effect,
    _slot: OwnedSemaphorePermit,
}
#[derive(Default)]
struct PendingState {
    commands: HashMap<u64, Pending>,
    late_success: u64,
    late_error: u64,
    ignored_replies: u64,
}
#[cfg(test)]
struct ReplyGate {
    id: u64,
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Mode {
    Running,
    Closing,
    Stopped(Fault),
}
struct Inner {
    scope: u64,
    pending: Mutex<PendingState>,
    next: AtomicU64,
    event_sequence: AtomicU64,
    navigations: Mutex<HashMap<ContextId, String>>,
    fenced: AtomicBool,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    mode: watch::Sender<Mode>,
    workers: watch::Sender<usize>,
    #[cfg(test)]
    reply_gate: Mutex<Option<ReplyGate>>,
    #[cfg(test)]
    semantic_realm: AtomicU64,
}
struct Worker(Arc<Inner>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.workers.send_modify(|count| *count -= 1);
    }
}

struct QueuedEvent {
    sequence: u64,
    event: Event,
    _bytes: OwnedSemaphorePermit,
}
pub(super) struct Events {
    inner: Arc<Inner>,
    queue: mpsc::Receiver<QueuedEvent>,
    delivered: u64,
}
impl Events {
    pub async fn recv(&mut self) -> Result<Event> {
        let mut mode = self.inner.mode.subscribe();
        loop {
            if let Mode::Stopped(fault) = *mode.borrow_and_update() {
                return Err(Error::Transport(fault));
            }
            tokio::select! {
                biased;
                _ = mode.changed() => continue,
                event = self.queue.recv() => {
                    let event = event.ok_or(Error::Transport(Fault::EventGap))?;
                    self.delivered = event.sequence;
                    return Ok(event.event);
                }
            }
        }
    }
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        self.inner.finish(Fault::EventGap);
    }
}

#[derive(Clone)]
pub(super) struct Bidi {
    inner: Arc<Inner>,
    outgoing: mpsc::Sender<String>,
    submission: Submission,
}

impl Bidi {
    pub fn new<S>(socket: WebSocketStream<S>) -> (Self, Events)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut write, mut read) = socket.split();
        let (outgoing, mut requests) = mpsc::channel::<String>(SLOTS);
        let (events, queue) = mpsc::channel(EVENT_COUNT);
        let (mode, mut reader_mode) = watch::channel(Mode::Running);
        let (workers, _) = watch::channel(2);
        let inner = Arc::new(Inner {
            scope: NEXT_SCOPE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .expect("BiDi connection identity exhausted"),
            pending: Mutex::new(PendingState::default()),
            next: AtomicU64::new(1),
            event_sequence: AtomicU64::new(0),
            navigations: Mutex::new(HashMap::new()),
            fenced: AtomicBool::new(false),
            slots: Arc::new(Semaphore::new(SLOTS)),
            bytes: Arc::new(Semaphore::new(EVENT_BYTES)),
            mode,
            workers,
            #[cfg(test)]
            reply_gate: Mutex::new(None),
            #[cfg(test)]
            semantic_realm: AtomicU64::new(0),
        });
        let reader = inner.clone();
        let worker = Worker(inner.clone());
        tokio::spawn(async move {
            let _worker = worker;
            loop {
                let message = tokio::select! {
                    biased;
                    _ = reader_mode.changed() => {
                        if matches!(*reader_mode.borrow_and_update(),Mode::Stopped(_)) { break; }
                        continue;
                    },
                    message = read.next() => message,
                };
                let fault = match message {
                    Some(Ok(Message::Text(text))) => {
                        #[cfg(test)]
                        if let Some(gate) = reader.take_reply_gate(&text) {
                            let _ = gate.reached.send(());
                            tokio::select! {
                                biased;
                                _ = reader_mode.changed() => {},
                                _ = gate.release => {},
                            }
                            if matches!(*reader_mode.borrow_and_update(), Mode::Stopped(_)) {
                                break;
                            }
                        }
                        if text.len() > MAX_MESSAGE {
                            Some(Fault::MessageLimit)
                        } else {
                            reader.receive(&text, &events).err()
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => None,
                    Some(Ok(Message::Close(_))) | None => Some(Fault::Disconnected),
                    Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                        Some(Fault::MessageLimit)
                    }
                    Some(Err(_)) => Some(Fault::Disconnected),
                    _ => Some(Fault::Malformed),
                };
                if let Some(fault) = fault {
                    reader.finish(fault);
                    break;
                }
            }
        });
        let writer = inner.clone();
        let worker = Worker(inner.clone());
        let mut writer_mode = inner.mode.subscribe();
        tokio::spawn(async move {
            let _worker = worker;
            let mut close_sent = false;
            loop {
                let mode = *writer_mode.borrow_and_update();
                match mode {
                    Mode::Stopped(_) => break,
                    Mode::Closing => {
                        if !close_sent {
                            // Dedicated control path; no ordinary slot or queue
                            // capacity is required to request native shutdown.
                            let close = Message::Text(
                                "{\"id\":0,\"method\":\"browser.close\",\"params\":{}}".into(),
                            );
                            let sent = tokio::select! {
                                biased;
                                _ = writer_mode.changed() => continue,
                                sent = tokio::time::timeout(Duration::from_millis(500),write.send(close)) => sent,
                            };
                            if !matches!(sent, Ok(Ok(()))) {
                                writer.finish(Fault::WriteTimeout);
                                break;
                            }
                            close_sent = true;
                        }
                        tokio::select! {
                            _ = writer_mode.changed() => continue,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => { writer.finish(Fault::Closed); break; }
                        }
                    }
                    Mode::Running => {
                        let frame = tokio::select! {
                            biased;
                            _ = writer_mode.changed() => continue,
                            frame = requests.recv() => match frame { Some(frame) => frame, None => { writer.finish(Fault::Closed); break; } },
                        };
                        let sent = tokio::select! {
                            biased;
                            _ = writer_mode.changed() => continue,
                            sent = tokio::time::timeout(Duration::from_secs(3),write.send(Message::Text(frame.into()))) => sent,
                        };
                        if !matches!(sent, Ok(Ok(()))) {
                            writer.finish(Fault::WriteTimeout);
                            break;
                        }
                    }
                }
            }
        });
        let client = Self {
            inner: inner.clone(),
            outgoing,
            submission: Submission::default(),
        };
        (
            client,
            Events {
                inner,
                queue,
                delivered: 0,
            },
        )
    }

    pub fn submit(&self, command: Command, deadline: Instant) -> Result<Call> {
        let reject = |code| Error::Rejected(Failure::new(code));
        if tokio::time::Instant::now() >= deadline.into() {
            return Err(reject("browser_deadline"));
        }
        if !self.connected() {
            return Err(reject("browser_lost"));
        }
        if !command.belongs_to(self.inner.scope) {
            return Err(reject("browser_context_scope"));
        }
        let slot = self
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| reject("browser_busy"))?;
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        if id > MAX_ID {
            self.inner.finish(Fault::Closed);
            return Err(reject("browser_id_exhausted"));
        }
        let (method, params, effect) = command.encode();
        #[cfg(test)]
        let params = {
            let mut params = params;
            let variant = self.inner.semantic_realm.load(Ordering::Acquire);
            if variant > 0 && params["target"]["sandbox"] == "rdh-semantic-dom-v1" {
                params["target"]["sandbox"] = json!(format!("rdh-semantic-dom-v1-{variant}"));
            }
            params
        };
        let frame = serde_json::to_string(&json!({"id":id,"method":method,"params":params}))
            .map_err(|_| reject("browser_invalid_params"))?;
        if frame.len() > MAX_OUTGOING {
            return Err(reject("browser_invalid_params"));
        }
        let receipt = Receipt {
            id,
            status: Arc::new(Mutex::new(Settlement::Enqueued)),
        };
        let (reply, result) = oneshot::channel();
        self.submission
            .enqueue(effect, || {
                let mut pending = self.inner.pending.lock().unwrap();
                if self.inner.fenced.load(Ordering::Acquire) {
                    return Err(Failure::new("browser_lost"));
                }
                pending.commands.insert(
                    id,
                    Pending {
                        reply,
                        receipt: receipt.clone(),
                        effect,
                        _slot: slot,
                    },
                );
                if self.outgoing.try_send(frame).is_err() {
                    pending.commands.remove(&id);
                    return Err(Failure::new("browser_busy"));
                }
                Ok(())
            })
            .map_err(Error::Rejected)?;
        Ok(Call {
            receipt,
            result,
            submission: self.submission.clone(),
            deadline,
        })
    }
    pub async fn call(&self, command: Command, deadline: Instant) -> Result<Reply> {
        self.submit(command, deadline)?.wait().await
    }
    pub fn authorized(&self, control: Arc<Control>, epoch: u64, paused: bool) -> Self {
        Self {
            submission: self.submission.authorized(control, epoch, paused),
            ..self.clone()
        }
    }
    pub fn track_effects(&self) -> (Self, Arc<AtomicBool>) {
        let (submission, flag) = self.submission.track_effects();
        (
            Self {
                submission,
                ..self.clone()
            },
            flag,
        )
    }
    /// The sole page input task owns all sources for this context. It calls
    /// this only after an input command may have been admitted; no new source
    /// or ordinary action can be submitted through this cleanup entry point.
    pub async fn release_inputs(&self, context: ContextId, deadline: Instant) -> Result<Reply> {
        let cleanup = Self {
            submission: self.submission.cleanup(),
            ..self.clone()
        };
        cleanup.call(Command::ReleaseInput(context), deadline).await
    }
    pub fn connected(&self) -> bool {
        !self.inner.fenced.load(Ordering::Acquire)
    }
    pub fn scope(&self) -> u64 {
        self.inner.scope
    }
    pub fn context(&self, value: &Value) -> Option<ContextId> {
        ContextId::parse(self.inner.scope, value)
    }
    pub fn navigation(&self, context: &ContextId) -> Result<Option<String>> {
        if !context.belongs_to(self.inner.scope) || !self.connected() {
            return Err(Error::Rejected(Failure::new("browser_lost")));
        }
        Ok(self.inner.navigations.lock().unwrap().get(context).cloned())
    }
    pub fn pending_effects(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap()
            .commands
            .values()
            .filter(|command| command.effect == Effect::Page)
            .count()
    }
    pub fn graceful_stop(&self) {
        self.inner.close();
    }
    pub fn stop(&self) {
        self.inner.finish(Fault::Closed);
    }
    pub fn lifecycle(&self) -> watch::Receiver<Mode> {
        self.inner.mode.subscribe()
    }
    #[cfg(test)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn advance_semantic_realm(&self) {
        self.inner.semantic_realm.fetch_add(1, Ordering::AcqRel);
    }
    #[cfg(test)]
    pub fn hold_next_reply(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, ready) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let mut gate = self.inner.reply_gate.lock().unwrap();
        assert!(gate.is_none());
        *gate = Some(ReplyGate {
            id: self.inner.next.load(Ordering::Relaxed),
            reached,
            release: wait,
        });
        (ready, release)
    }
    pub async fn wait_stopped(&self, deadline: Instant) -> Result<()> {
        let mut workers = self.inner.workers.subscribe();
        tokio::time::timeout_at(deadline.into(), async {
            while *workers.borrow_and_update() > 0 {
                workers.changed().await.map_err(|_| Error::CleanupPending)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| Error::CleanupPending)?
    }
    pub fn snapshot(&self) -> Value {
        let pending = self.inner.pending.lock().unwrap();
        json!({"pending":pending.commands.len(),"effects":pending.commands.values().filter(|c|c.effect==Effect::Page).count(),"free_slots":self.inner.slots.available_permits(),"event_bytes":EVENT_BYTES-self.inner.bytes.available_permits(),"late_success":pending.late_success,"late_error":pending.late_error,"ignored_replies":pending.ignored_replies,"workers":*self.inner.workers.borrow()})
    }
}

impl Inner {
    #[cfg(test)]
    fn take_reply_gate(&self, text: &str) -> Option<ReplyGate> {
        let mut gate = self.reply_gate.lock().unwrap();
        let id = gate.as_ref()?.id;
        let value: Value = serde_json::from_str(text).ok()?;
        (value["id"].as_u64() == Some(id))
            .then(|| gate.take())
            .flatten()
    }
    fn receive(
        &self,
        text: &str,
        events: &mpsc::Sender<QueuedEvent>,
    ) -> std::result::Result<(), Fault> {
        let value: Value = serde_json::from_str(text).map_err(|_| Fault::Malformed)?;
        match value["type"].as_str() {
            Some("success" | "error") => {
                let id = value["id"]
                    .as_u64()
                    .filter(|id| *id <= MAX_ID)
                    .ok_or(Fault::Malformed)?;
                let result = if value["type"] == "success" {
                    if !value["result"].is_object() {
                        return Err(Fault::Malformed);
                    }
                    Ok(Reply {
                        value: value["result"].clone(),
                        event_boundary: self.event_sequence.load(Ordering::Acquire),
                    })
                } else {
                    let code = value["error"]
                        .as_str()
                        .filter(|s| !s.is_empty() && s.len() <= 256)
                        .ok_or(Fault::Malformed)?;
                    if !value["message"].is_string() {
                        return Err(Fault::Malformed);
                    }
                    Err(Error::Protocol(ProtocolError::parse(code)))
                };
                let mut pending = self.pending.lock().unwrap();
                if let Some(command) = pending.commands.remove(&id) {
                    let status = match &result {
                        Ok(_) => Settlement::Success,
                        Err(Error::Protocol(code)) => Settlement::ProtocolError(*code),
                        _ => unreachable!(),
                    };
                    if command.reply.is_closed() {
                        if result.is_ok() {
                            pending.late_success += 1;
                        } else {
                            pending.late_error += 1;
                        }
                    }
                    command.receipt.settle(status);
                    let _ = command.reply.send(result);
                } else {
                    pending.ignored_replies += 1;
                }
            }
            Some("event") => {
                let method = value["method"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                    .ok_or(Fault::Malformed)?;
                if !value["params"].is_object() {
                    return Err(Fault::Malformed);
                }
                // Only subscribed lifecycle events enter the critical channel.
                // Future diagnostic subscriptions need separate budgets.
                if !CORE_EVENTS.contains(&method) && !super::event::PAGE_EVENTS.contains(&method) {
                    return Ok(());
                }
                let event =
                    Event::parse(self.scope, method, &value["params"]).ok_or(Fault::Malformed)?;
                let mut pending = self.pending.lock().unwrap();
                let sent = (|| {
                    // Read-side publication precedes replies on this connection;
                    // a slow event consumer cannot hide a committed navigation
                    // from a later native document query (including BFCache).
                    match &event {
                        Event::Navigation {
                            context,
                            navigation,
                        } => {
                            let mut current = self.navigations.lock().unwrap();
                            if !current.contains_key(context)
                                && current.len() >= super::event::MAX_CONTEXTS
                            {
                                return Err(Fault::EventGap);
                            }
                            current.insert(context.clone(), navigation.clone());
                        }
                        Event::Destroyed { context } => {
                            self.navigations.lock().unwrap().remove(context);
                        }
                        _ => {}
                    }
                    if text.len() > MAX_EVENT {
                        return Err(Fault::EventGap);
                    }
                    let bytes = self
                        .bytes
                        .clone()
                        .try_acquire_many_owned(text.len() as u32)
                        .map_err(|_| Fault::EventGap)?;
                    let sequence = self.event_sequence.fetch_add(1, Ordering::AcqRel) + 1;
                    events
                        .try_send(QueuedEvent {
                            sequence,
                            event,
                            _bytes: bytes,
                        })
                        .map_err(|_| Fault::EventGap)
                })();
                if let Err(fault) = sent {
                    // The queue-failure decision and ordinary admission share
                    // one lock. No consumer recv is required to fence inputs.
                    self.finish_locked(&mut pending, fault);
                    return Err(fault);
                }
            }
            _ => return Err(Fault::Malformed),
        }
        Ok(())
    }
    fn close(&self) {
        let _pending = self.pending.lock().unwrap();
        if self.fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        self.slots.close();
        self.mode.send_replace(Mode::Closing);
        // Existing commands retain their permits and receipts until a reply or
        // transport termination; closing admission cannot invent settlement.
    }
    fn finish(&self, fault: Fault) {
        let mut pending = self.pending.lock().unwrap();
        self.finish_locked(&mut pending, fault);
    }
    fn finish_locked(&self, pending: &mut PendingState, fault: Fault) {
        if matches!(*self.mode.borrow(), Mode::Stopped(_)) {
            return;
        }
        self.fenced.store(true, Ordering::Release);
        self.slots.close();
        self.mode.send_replace(Mode::Stopped(fault));
        self.navigations.lock().unwrap().clear();
        for (_, command) in pending.commands.drain() {
            command.receipt.settle(Settlement::Unknown(fault));
            let _ = command.reply.send(Err(Error::Transport(fault)));
        }
    }
}
