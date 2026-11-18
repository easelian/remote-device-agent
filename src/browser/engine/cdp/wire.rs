//! Private Chromium pipe, with bounded framing, queueing and late-reply ownership.
#[cfg(feature = "experimental-browser-bidi")]
use crate::browser::semantic::task_events::{Fact, FailureKind, Sink};
use crate::browser::{
    Failure, Result,
    control::{Control, Effect, Submission},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot, watch},
};

#[cfg(test)]
mod bounds_tests;
mod command;
#[cfg(test)]
mod event_tests;
mod events;
pub(in crate::browser::engine) use events::{EventLease, EventReceiver};
#[cfg(test)]
mod lease_tests;
mod leases;
#[cfg(test)]
mod session_tests;
mod sessions;
#[cfg(test)]
mod submission_tests;
use command::Dialog;
pub use command::{Command, InputPair};

#[cfg(test)]
mod metrics;
const MAX_MESSAGE: usize = 2 * 1024 * 1024;
pub(in crate::browser::engine) struct NativeReply {
    value: Value,
    leases: Vec<leases::Reap>,
    owner: std::sync::Weak<Inner>,
    outgoing: mpsc::WeakSender<Vec<u8>>,
}
impl NativeReply {
    #[cfg(feature = "experimental-browser-bidi")]
    pub fn value(&self) -> &Value {
        &self.value
    }
    fn accept(mut self) -> Value {
        self.leases.clear();
        std::mem::take(&mut self.value)
    }
}
impl Drop for NativeReply {
    fn drop(&mut self) {
        if !self.leases.is_empty()
            && let Some(owner) = self.owner.upgrade()
        {
            owner.reap(&self.leases, &self.outgoing);
        }
    }
}

struct Pending {
    lease: leases::LeaseCommand,
    cleanup_deadline: Option<tokio::time::Instant>,
    retired_deadline: Option<tokio::time::Instant>,
    #[cfg(test)]
    metric: metrics::Command,
    reply: Option<oneshot::Sender<Result<NativeReply>>>,
    session: Option<String>,
    dialog_blocks: bool,
    input_barrier: bool,
    _permit: OwnedSemaphorePermit,
}
#[derive(Default)]
struct PendingState {
    leases: leases::Book,
    sessions: sessions::Sessions,
    #[cfg(test)]
    resources: metrics::Resources,
    commands: HashMap<u64, Pending>,
    dialogs: HashSet<String>,
    event_sequence: u64,
    #[cfg(test)]
    event_peak: (usize, usize),
    event_receivers: usize,
    event_deadline: Option<tokio::time::Instant>,
}
struct Inner {
    pending: Mutex<PendingState>,
    next: AtomicU64,
    closed: AtomicBool,
    fenced: AtomicBool,
    event_slots: Arc<Semaphore>,
    event_bytes: Arc<Semaphore>,
    #[cfg(feature = "experimental-browser-bidi")]
    task_events: Mutex<Option<(String, Sink)>>,
    slots: Arc<Semaphore>,
    cleanup_slots: Arc<Semaphore>,
    stop: watch::Sender<Shutdown>,
    #[cfg(test)]
    reply_gate: Mutex<Option<ReplyGate>>,
    #[cfg(test)]
    barriers: AtomicU64,
}
#[cfg(test)]
struct ReplyGate {
    id: u64,
    method: Option<&'static str>,
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shutdown {
    Running,
    Fenced,
    Graceful,
    Immediate,
}

#[derive(Clone)]
pub struct Cdp {
    inner: Arc<Inner>,
    outgoing: mpsc::Sender<Vec<u8>>,
    events: broadcast::Sender<Arc<events::Envelope>>,
    submission: Submission,
}

enum Admission {
    Sent { blocked_release: bool },
    RetiredObject,
}

impl Cdp {
    #[cfg(all(test, feature = "experimental-browser-bidi"))]
    pub fn hold_next_reply(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, ready) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let mut gate = self.inner.reply_gate.lock().unwrap();
        assert!(gate.is_none());
        *gate = Some(ReplyGate {
            id: self.inner.next.load(Ordering::Relaxed),
            method: None,
            reached,
            release: wait,
        });
        (ready, release)
    }
    #[cfg(all(test, feature = "experimental-browser-bidi"))]
    pub fn hold_next_reply_for(
        &self,
        command: Command,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, ready) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let mut gate = self.inner.reply_gate.lock().unwrap();
        assert!(gate.is_none());
        *gate = Some(ReplyGate {
            id: 0,
            method: Some(command.spec().method),
            reached,
            release: wait,
        });
        (ready, release)
    }
    #[cfg(test)]
    pub fn test_metrics(&self) -> Value {
        let pending = self.inner.pending.lock().unwrap();
        json!({"commands":self.inner.next.load(Ordering::Relaxed)-1,"pending":pending.commands.len(),"barriers":self.inner.barriers.load(Ordering::Acquire),"native_objects":pending.resources.objects.len(),"streams":pending.resources.streams.len(),"handle_inventory":pending.resources.snapshot(),"leases":pending.leases.snapshot(),"cleanup_pending":pending.commands.values().filter(|c| c.cleanup_deadline.is_some()).count(),"retired_pending":pending.commands.values().filter(|c| c.retired_deadline.is_some()).count(),"sessions":pending.sessions.snapshot(),"events":{"count":events::MAX_EVENTS-self.inner.event_slots.available_permits(),"bytes":events::MAX_EVENT_BYTES-self.inner.event_bytes.available_permits(),"peak_count":pending.event_peak.0,"peak_bytes":pending.event_peak.1,"receivers":pending.event_receivers,"sequence":pending.event_sequence,"fenced":self.inner.fenced.load(Ordering::Acquire)}})
    }
    #[cfg(test)]
    pub fn command_count(&self) -> u64 {
        self.inner.next.load(Ordering::Relaxed) - 1
    }
    pub fn new<R, W>(mut read: R, mut write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing, mut requests) = mpsc::channel::<Vec<u8>>(32 + leases::CLEANUP_SLOTS);
        let (events, _) = broadcast::channel(events::MAX_EVENTS);
        let (stop, mut stopped) = watch::channel(Shutdown::Running);
        let inner = Arc::new(Inner {
            pending: Mutex::new(PendingState::default()),
            next: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            event_slots: Arc::new(Semaphore::new(events::MAX_EVENTS)),
            event_bytes: Arc::new(Semaphore::new(events::MAX_EVENT_BYTES)),
            #[cfg(feature = "experimental-browser-bidi")]
            task_events: Mutex::new(None),
            slots: Arc::new(Semaphore::new(32)),
            cleanup_slots: Arc::new(Semaphore::new(leases::CLEANUP_SLOTS)),
            stop,
            #[cfg(test)]
            reply_gate: Mutex::new(None),
            #[cfg(test)]
            barriers: AtomicU64::new(0),
        });
        let reader = inner.clone();
        let notifications = events.clone();
        let cleanup_queue = outgoing.downgrade();
        tokio::spawn(async move {
            let mut frame = Vec::new();
            let mut chunk = [0u8; 16384];
            let mut cleanup_tick = tokio::time::interval(Duration::from_millis(100));
            'reader: loop {
                let count = tokio::select! {
                    _ = cleanup_tick.tick() => {
                        let mut pending = reader.pending.lock().unwrap();
                        let now = tokio::time::Instant::now();
                        if pending.event_deadline.is_some_and(|d| now >= d) || pending.commands.values().any(|c|c.cleanup_deadline.is_some_and(|d|now>=d)) { break; }
                        if pending.commands.values().any(|c| c.retired_deadline.is_some_and(|d| now >= d)) {
                            // Local retirement cannot free unacknowledged work.
                            // Escalate to the existing owned-browser close path.
                            reader.fence_events(&mut pending);
                        }
                        continue;
                    },
                    _ = stopped.changed() => {
                        if *stopped.borrow_and_update() == Shutdown::Immediate { break; }
                        continue;
                    },
                    result = read.read(&mut chunk) => match result { Ok(0) | Err(_) => break, Ok(n) => n },
                };
                for byte in &chunk[..count] {
                    if *byte == 0 {
                        let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
                            break 'reader;
                        };
                        let encoded_bytes = frame.len();
                        frame.clear();
                        if let Some(id) = value["id"].as_u64() {
                            #[cfg(test)]
                            {
                                let gate = {
                                    let mut gate = reader.reply_gate.lock().unwrap();
                                    if gate.as_ref().is_some_and(|gate| gate.id == id) {
                                        gate.take()
                                    } else {
                                        None
                                    }
                                };
                                if let Some(gate) = gate {
                                    let _ = gate.reached.send(());
                                    tokio::select! {_ = stopped.changed()=>{},_ = gate.release=>{}}
                                    if *stopped.borrow_and_update() == Shutdown::Immediate {
                                        break 'reader;
                                    }
                                }
                            }
                            let (command, created) = {
                                let mut pending = reader.pending.lock().unwrap();
                                let command = pending.commands.remove(&id);
                                let created = if let Some(command) = &command {
                                    #[cfg(test)]
                                    pending.resources.settle(&command.metric, &value);
                                    let mut created = pending.leases.settle(&command.lease, &value);
                                    if command.retired_deadline.is_some()
                                        && let Some(session) = command.session.as_deref()
                                    {
                                        pending.leases.retire_objects(session);
                                        #[cfg(test)]
                                        pending.resources.retire_objects(session);
                                        created.retain(|resource| pending.leases.owns(resource));
                                    }
                                    created
                                } else {
                                    Vec::new()
                                };
                                (command, created)
                            };
                            if command.is_none() && leases::Book::unexpected_reply(&value) {
                                break 'reader;
                            }
                            if let Some(command) = command {
                                if command.cleanup_deadline.is_some()
                                    && value.get("error").is_some()
                                    && !(command.retired_deadline.is_some()
                                        && command.lease.object_cleanup())
                                {
                                    break 'reader;
                                }
                                let result = if value.get("error").is_some() {
                                    Err(Failure::unknown("browser_command_failed"))
                                } else {
                                    let mut reply = NativeReply {
                                        value: value["result"].clone(),
                                        leases: created,
                                        owner: Arc::downgrade(&reader),
                                        outgoing: cleanup_queue.clone(),
                                    };
                                    // Script exceptions are never exposed as usable native
                                    // objects by the semantic API, even to a live caller.
                                    if reply.value.get("exceptionDetails").is_some() {
                                        reader.reap(&reply.leases, &cleanup_queue);
                                        reply.leases.clear();
                                    }
                                    Ok(reply)
                                };
                                if let Some(reply) = command.reply {
                                    // NativeReply also reaps when delivery succeeded but
                                    // the receiver was dropped before consuming it.
                                    let _ = reply.send(result);
                                }
                            }
                            #[cfg(test)]
                            if id == 0 {
                                reader.barriers.fetch_add(1, Ordering::Release);
                            }
                        } else {
                            #[cfg(feature = "experimental-browser-bidi")]
                            let task_fact;
                            {
                                let mut pending = reader.pending.lock().unwrap();
                                #[cfg(test)]
                                pending.resources.event(&value);
                                // Validate ownership even for messages that no consumer retains.
                                if !pending.leases.event(&value)
                                    || !reader.session_event(&mut pending, &value)
                                {
                                    reader.fence_events(&mut pending);
                                    continue;
                                }
                                #[cfg(feature = "experimental-browser-bidi")]
                                {
                                    task_fact = reader.capture_event(&value);
                                }
                                reader.publish_event(
                                    &mut pending,
                                    &notifications,
                                    value,
                                    encoded_bytes,
                                );
                            }
                            #[cfg(feature = "experimental-browser-bidi")]
                            if let Some((sink, fact)) = task_fact {
                                sink.push(fact);
                            }
                        }
                    } else {
                        if frame.len() == MAX_MESSAGE {
                            break 'reader;
                        }
                        frame.push(*byte);
                    }
                }
            }
            reader.finish();
        });
        let writer = inner.clone();
        let mut stopped = inner.stop.subscribe();
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = stopped.changed() => {
                        let state = *stopped.borrow_and_update();
                        if state == Shutdown::Fenced { continue; }
                        let graceful = state == Shutdown::Graceful;
                        if graceful {
                            // Fence ordinary commands immediately, but retain the read
                            // pipe while Chromium flushes profile state and exits.
                            let close = b"{\"id\":0,\"method\":\"Browser.close\",\"params\":{}}\0";
                            if matches!(tokio::time::timeout(Duration::from_millis(500), write.write_all(close)).await, Ok(Ok(()))) {
                                tokio::select! {
                                    _ = stopped.changed() => {},
                                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                                }
                            }
                        }
                        break;
                    },
                    frame = requests.recv() => match frame { Some(frame) => frame, None => break },
                };
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(3), write.write_all(&frame)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
            writer.finish();
        });
        Self {
            inner,
            outgoing,
            events,
            submission: Submission::default(),
        }
    }

    #[cfg(feature = "experimental-browser-bidi")]
    pub fn set_task_events(&self, session: &str, sink: Sink) -> Result<()> {
        let mut slot = self.inner.task_events.lock().unwrap();
        if let Some((_, previous)) = slot.take() {
            // A new subscription is a new document window. Retire the old
            // sink before replacing it so facts cannot cross navigation.
            previous.invalidate();
        }
        *slot = Some((session.to_owned(), sink));
        Ok(())
    }

    pub async fn call(
        &self,
        session: Option<&str>,
        command: Command,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.scoped_call(session, command, params, deadline)
            .await
            .map(NativeReply::accept)
    }

    /// Keep ownership of objects returned by this call until the scope ends.
    /// Drop queues cleanup without erasing the charge before its native reply.
    pub(in crate::browser::engine) async fn scoped_call(
        &self,
        session: Option<&str>,
        command: Command,
        params: Value,
        deadline: Instant,
    ) -> Result<NativeReply> {
        let spec = command.spec();
        if Instant::now() >= deadline {
            return Err(Failure::new("browser_deadline"));
        }
        if !self.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        let permit = if leases::LeaseCommand::cleanup(command) {
            &self.inner.cleanup_slots
        } else {
            &self.inner.slots
        }
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::new("browser_busy"))?;
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        {
            let mut gate = self.inner.reply_gate.lock().unwrap();
            if let Some(gate) = gate.as_mut()
                && gate.method == Some(spec.method)
            {
                gate.id = id;
                gate.method = None;
            }
        }
        #[cfg(test)]
        let metric = metrics::Command::new(session, spec.method, &params);
        let mut message = json!({"id":id,"method":spec.method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let mut frame =
            serde_json::to_vec(&message).map_err(|_| Failure::new("browser_invalid_params"))?;
        if frame.len() > 256 * 1024 {
            return Err(Failure::new("browser_invalid_params"));
        }
        frame.push(0);
        let (reply, result) = oneshot::channel();
        let admission = self.submission.enqueue(spec.effect, || {
            let mut pending = self.inner.pending.lock().unwrap();
            if !self.connected() {
                return Err(Failure::unknown("browser_lost"));
            }
            if let Some(session) = session
                && !pending.sessions.contains(session)
            {
                if matches!(
                    command,
                    Command::RuntimeReleaseObject | Command::RuntimeReleaseObjectGroup
                ) {
                    // RemoteObject IDs belong to the detached debugging session.
                    // No such object remains usable, even after a late reply.
                    return Ok(Admission::RetiredObject);
                }
                if matches!(command, Command::IoClose)
                    && pending.leases.owns(&leases::Reap::Stream {
                        session: session.into(),
                        id: message["params"]["handle"].as_str().unwrap_or("").into(),
                    })
                {
                    // IO storage is target-owned, not proof of session-owned
                    // cleanup. Keep its charge and recover the owned browser.
                    self.inner.fence_events(&mut pending);
                    return Err(Failure::unknown("browser_cleanup_pending"));
                }
                return Err(Failure::new("browser_stale_frame"));
            }
            let dialog_blocks = spec.dialog != Dialog::Continue;
            let dialog_open = session.is_some_and(|s| pending.dialogs.contains(s));
            if dialog_open && spec.dialog == Dialog::Block {
                return Err(Failure::new("browser_dialog_open"));
            }
            let lease =
                leases::LeaseCommand::new(command, session, &message["params"], &pending.leases);
            pending.leases.reserve(&lease)?;
            pending.commands.insert(
                id,
                Pending {
                    lease,
                    cleanup_deadline: None,
                    retired_deadline: None,
                    #[cfg(test)]
                    metric,
                    reply: Some(reply),
                    session: session.map(str::to_owned),
                    dialog_blocks,
                    input_barrier: spec.effect == Effect::Page,
                    _permit: permit,
                },
            );
            if self.outgoing.try_send(frame).is_err() {
                if let Some(command) = pending.commands.remove(&id) {
                    pending.leases.cancel_unsent(&command.lease);
                }
                return Err(Failure::new("browser_busy"));
            }
            Ok(Admission::Sent {
                blocked_release: dialog_open && dialog_blocks,
            })
        })?;
        match admission {
            Admission::RetiredObject => {
                return Ok(NativeReply {
                    value: json!({}),
                    leases: Vec::new(),
                    owner: Arc::downgrade(&self.inner),
                    outgoing: self.outgoing.downgrade(),
                });
            }
            Admission::Sent {
                blocked_release: true,
            } => return Err(Failure::unknown("browser_dialog_open")),
            Admission::Sent {
                blocked_release: false,
            } => {}
        }
        // Timeout never releases the outstanding command's slot or implies it stopped.
        match tokio::time::timeout_at(deadline.into(), result).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Failure::unknown("browser_lost")),
            Err(_) => Err(Failure::unknown("browser_timeout_unknown")),
        }
    }

    /// Release only a press that may have reached the browser. Any failure after
    /// that point describes an uncertain input outcome, including failed cleanup.
    pub async fn input_pair(
        &self,
        session: Option<&str>,
        pair: InputPair,
        press: Value,
        release: Value,
        deadline: Instant,
    ) -> Result<()> {
        let (press_command, release_command) = pair.commands();
        let pressed = self.call(session, press_command, press, deadline).await;
        if pressed
            .as_ref()
            .is_err_and(|error| error.state == "not_sent")
        {
            return pressed.map(|_| ());
        }
        let released = self
            .cleanup()
            .call(
                session,
                release_command,
                release,
                deadline.max(Instant::now() + Duration::from_secs(1)),
            )
            .await;
        pressed.and(released).map(|_| ()).map_err(|mut error| {
            error.state = "unknown";
            error
        })
    }
    pub fn connected(&self) -> bool {
        !self.inner.closed.load(Ordering::Acquire) && !self.inner.fenced.load(Ordering::Acquire)
    }
    pub fn pending(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap()
            .commands
            .values()
            .filter(|p| p.input_barrier)
            .count()
    }
    pub async fn close_stream(
        &self,
        session: Option<&str>,
        handle: &str,
        deadline: Instant,
    ) -> Result<()> {
        let wire = self.cleanup();
        loop {
            let result = wire
                .call(
                    session,
                    Command::IoClose,
                    json!({"handle":handle}),
                    deadline,
                )
                .await;
            match result {
                // Bounded transfer jobs retain their stream while waiting for
                // the four existing cleanup slots. Only failed admission is
                // retried; an admitted close, including unknown completion,
                // must settle under its original command ID.
                Err(error) if error.code == "browser_busy" && error.state == "not_sent" => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(Failure::new("browser_deadline"));
                    }
                    tokio::time::sleep(remaining.min(Duration::from_millis(1))).await;
                }
                result => return result.map(drop),
            }
        }
    }

    pub fn stop(&self) {
        self.inner.finish();
    }
    pub fn graceful_stop(&self) {
        self.inner.shutdown(Shutdown::Graceful);
    }
    pub fn authorized(&self, control: Arc<Control>, epoch: u64) -> Self {
        let mut copy = self.clone();
        copy.submission = self.submission.authorized(control, epoch, false);
        copy
    }
    pub fn human(&self, control: Arc<Control>, epoch: u64) -> Self {
        let mut copy = self.clone();
        copy.submission = self.submission.authorized(control, epoch, true);
        copy
    }
    pub fn track_effects(&self) -> (Self, Arc<AtomicBool>) {
        let (submission, flag) = self.submission.track_effects();
        let mut copy = self.clone();
        copy.submission = submission;
        (copy, flag)
    }
    pub fn cleanup(&self) -> Self {
        let mut copy = self.clone();
        copy.submission = self.submission.cleanup();
        copy
    }
}

impl Inner {
    fn reap(&self, resources: &[leases::Reap], outgoing: &mpsc::WeakSender<Vec<u8>>) {
        if resources.is_empty() {
            return;
        }
        // The private reader must not keep the ordinary send side alive after
        // the last public client has gone away.
        let Some(outgoing) = outgoing.upgrade() else {
            self.finish();
            return;
        };
        let success = (|| {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return true;
            }
            for resource in resources {
                if !pending.leases.owns(resource) {
                    continue;
                }
                if let leases::Reap::Stream { session, .. } = resource
                    && !session.is_empty()
                    && !pending.sessions.contains(session)
                {
                    self.fence_events(&mut pending);
                    return true;
                }
                let Ok(permit) = self.cleanup_slots.clone().try_acquire_owned() else {
                    return false;
                };
                let (session, command, params) = resource.command();
                let id = self.next.fetch_add(1, Ordering::Relaxed);
                let lease = leases::LeaseCommand::new(command, session, &params, &pending.leases);
                #[cfg(test)]
                let metric = metrics::Command::new(session, command.spec().method, &params);
                let mut value = json!({"id":id,"method":command.spec().method,"params":params});
                if let Some(session) = session.filter(|s| !s.is_empty()) {
                    value["sessionId"] = session.into();
                }
                let Ok(mut frame) = serde_json::to_vec(&value) else {
                    return false;
                };
                frame.push(0);
                pending.commands.insert(
                    id,
                    Pending {
                        lease,
                        cleanup_deadline: Some(
                            tokio::time::Instant::now() + Duration::from_secs(3),
                        ),
                        retired_deadline: None,
                        #[cfg(test)]
                        metric,
                        reply: None,
                        session: session.map(str::to_owned),
                        dialog_blocks: false,
                        input_barrier: false,
                        _permit: permit,
                    },
                );
                if outgoing.try_send(frame).is_err() {
                    pending.commands.remove(&id);
                    return false;
                }
            }
            true
        })();
        if !success {
            self.finish();
        }
    }

    fn finish(&self) {
        self.shutdown(Shutdown::Immediate);
    }
    fn shutdown(&self, how: Shutdown) {
        let mut pending = self.pending.lock().unwrap();
        // A transport failure may finish graceful shutdown. Repeated graceful
        // requests must not downgrade an already stopped transport.
        if self.closed.swap(true, Ordering::AcqRel) && how == Shutdown::Graceful {
            return;
        }
        self.stop.send_replace(how);
        self.slots.close();
        self.cleanup_slots.close();
        let commands: Vec<_> = pending
            .commands
            .drain()
            .map(|(_, command)| command)
            .collect();
        for command in commands {
            pending.leases.cancel_unsent(&command.lease);
            if let Some(reply) = command.reply {
                let _ = reply.send(Err(Failure::unknown("browser_lost")));
            }
        }
        pending.dialogs.clear();
        pending.sessions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn graceful_close_fences_work_and_keeps_pipe_for_browser_exit() {
        let (client, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(client);
        let cdp = Cdp::new(read, write);
        let worker = cdp.clone();
        let pending = tokio::spawn(async move {
            worker
                .call(
                    None,
                    Command::RuntimeEvaluate(Effect::None),
                    json!({}),
                    Instant::now() + Duration::from_secs(5),
                )
                .await
        });
        let mut server = BufReader::new(server);
        let mut frame = Vec::new();
        server.read_until(0, &mut frame).await.unwrap();
        cdp.graceful_stop();
        assert!(pending.await.unwrap().is_err());
        assert!(!cdp.connected());
        assert!(
            cdp.call(
                None,
                Command::PageNavigate,
                json!({}),
                Instant::now() + Duration::from_secs(1)
            )
            .await
            .is_err()
        );
        frame.clear();
        tokio::time::timeout(Duration::from_secs(1), server.read_until(0, &mut frame))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&frame[..frame.len() - 1]).unwrap()["method"],
            "Browser.close"
        );
        // The browser can still acknowledge close; no premature EOF before it
        // has persisted the profile and shut its own pipe down.
        server
            .get_mut()
            .write_all(b"{\"id\":0,\"result\":{}}\0")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), server.read_u8())
                .await
                .is_err()
        );
        cdp.stop();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), server.read_u8())
                .await
                .unwrap()
                .is_err()
        );
    }
}
