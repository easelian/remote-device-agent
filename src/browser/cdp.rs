//! Private Chromium pipe, with bounded framing, queueing and late-reply ownership.
use super::{Failure, Result};
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
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot, watch},
};

const MAX_MESSAGE: usize = 2 * 1024 * 1024;
struct Pending {
    reply: Option<oneshot::Sender<Result<Value>>>,
    session: Option<String>,
    dialog_blocks: bool,
    input_barrier: bool,
    _permit: OwnedSemaphorePermit,
}
#[derive(Default)]
struct PendingState {
    commands: HashMap<u64, Pending>,
    dialogs: HashSet<String>,
}
struct Inner {
    pending: Mutex<PendingState>,
    next: AtomicU64,
    closed: AtomicBool,
    slots: Arc<Semaphore>,
    stop: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Cdp {
    inner: Arc<Inner>,
    outgoing: mpsc::Sender<Vec<u8>>,
    events: broadcast::Sender<Value>,
    authorization: Option<(Arc<Control>, u64, bool)>,
    effects: Option<Arc<AtomicBool>>,
}

pub struct Control {
    // Serializes pause/close against the last authorization check and enqueue.
    pub state: Mutex<(u64, bool)>,
}
impl Control {
    pub fn new() -> Self {
        Self {
            state: Mutex::new((1, false)),
        }
    }
    pub fn change(&self, paused: bool) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        state.1 = paused;
        state.0
    }
    pub fn snapshot(&self) -> (u64, bool) {
        *self.state.lock().unwrap()
    }
    pub fn resume(&self, expected: u64) -> Result<u64> {
        let mut state = self.state.lock().unwrap();
        if state.0 != expected {
            return Err(Failure::new("browser_control_changed"));
        }
        state.0 += 1;
        state.1 = false;
        Ok(state.0)
    }
}

impl Cdp {
    pub fn new(mut read: UnixStream, mut write: UnixStream) -> Self {
        let (outgoing, mut requests) = mpsc::channel::<Vec<u8>>(32);
        let (events, _) = broadcast::channel(256);
        let (stop, mut stopped) = watch::channel(false);
        let inner = Arc::new(Inner {
            pending: Mutex::new(PendingState::default()),
            next: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            slots: Arc::new(Semaphore::new(32)),
            stop,
        });
        let reader = inner.clone();
        let notifications = events.clone();
        tokio::spawn(async move {
            let mut frame = Vec::new();
            let mut chunk = [0u8; 16384];
            'reader: loop {
                let count = tokio::select! {
                    _ = stopped.changed() => break,
                    result = read.read(&mut chunk) => match result { Ok(0) | Err(_) => break, Ok(n) => n },
                };
                for byte in &chunk[..count] {
                    if *byte == 0 {
                        let Ok(value) = serde_json::from_slice::<Value>(&frame) else {
                            break 'reader;
                        };
                        frame.clear();
                        if let Some(id) = value["id"].as_u64() {
                            if let Some(command) =
                                reader.pending.lock().unwrap().commands.remove(&id)
                            {
                                let result = if value.get("error").is_some() {
                                    Err(Failure::unknown("browser_command_failed"))
                                } else {
                                    Ok(value["result"].clone())
                                };
                                if let Some(reply) = command.reply {
                                    let _ = reply.send(result);
                                }
                            }
                        } else {
                            if let Some(session) = value["sessionId"].as_str() {
                                let mut pending = reader.pending.lock().unwrap();
                                match value["method"].as_str() {
                                    Some("Page.javascriptDialogOpening") => {
                                        pending.dialogs.insert(session.into());
                                        for command in pending.commands.values_mut().filter(|c| {
                                            c.session.as_deref() == Some(session) && c.dialog_blocks
                                        }) {
                                            // Keep the command and its permit until the real reply.
                                            if let Some(reply) = command.reply.take() {
                                                let _ = reply.send(Err(Failure::unknown(
                                                    "browser_dialog_open",
                                                )));
                                            }
                                        }
                                    }
                                    Some("Page.javascriptDialogClosed") => {
                                        pending.dialogs.remove(session);
                                    }
                                    _ => {}
                                }
                            }
                            let _ = notifications.send(value);
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
                    _ = stopped.changed() => break,
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
            authorization: None,
            effects: None,
        }
    }

    pub async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.send(session, method, params, deadline, false).await
    }
    pub async fn call_effect(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.send(session, method, params, deadline, true).await
    }
    async fn send(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        deadline: Instant,
        explicit_effect: bool,
    ) -> Result<Value> {
        if Instant::now() >= deadline {
            return Err(Failure::new("browser_deadline"));
        }
        let permit = self
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::new("browser_busy"))?;
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id":id,"method":method,"params":params});
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
        {
            let control = self
                .authorization
                .as_ref()
                .map(|(control, _, _)| control.state.lock().unwrap());
            if let Some((_, epoch, paused)) = &self.authorization
                && control
                    .as_ref()
                    .is_some_and(|state| state.0 != *epoch || state.1 != *paused)
            {
                return Err(Failure::new("browser_control_changed"));
            }
            let mut pending = self.inner.pending.lock().unwrap();
            if self.inner.closed.load(Ordering::Acquire) {
                return Err(Failure::unknown("browser_lost"));
            }
            let dialog_blocks = method.starts_with("Input.")
                || method.starts_with("DOM.")
                || method.starts_with("Page.") && method != "Page.handleJavaScriptDialog"
                || matches!(
                    method,
                    "Runtime.evaluate" | "Runtime.callFunctionOn" | "Page.navigate"
                );
            let dialog_open = session.is_some_and(|s| pending.dialogs.contains(s));
            let release = matches!(params["type"].as_str(), Some("keyUp" | "mouseReleased"));
            if dialog_open && dialog_blocks && !release {
                return Err(Failure::new("browser_dialog_open"));
            }
            pending.commands.insert(
                id,
                Pending {
                    reply: Some(reply),
                    session: session.map(str::to_owned),
                    dialog_blocks,
                    // Only commands that can emit input or change a page must
                    // drain before handing control to another actor. Background
                    // setup, reads and owned file streams retain transport slots.
                    input_barrier: explicit_effect
                        || method.starts_with("Input.")
                        || matches!(
                            method,
                            "Page.navigate"
                                | "Page.bringToFront"
                                | "DOM.focus"
                                | "DOM.scrollIntoViewIfNeeded"
                                | "Target.createTarget"
                                | "Target.closeTarget"
                                | "Page.handleJavaScriptDialog"
                        ),
                    _permit: permit,
                },
            );
            if self.outgoing.try_send(frame).is_err() {
                pending.commands.remove(&id);
                return Err(Failure::new("browser_busy"));
            }
            if let Some(effects) = &self.effects
                && (explicit_effect
                    || method.starts_with("Input.")
                    || matches!(
                        method,
                        "Page.navigate"
                            | "Page.bringToFront"
                            | "DOM.focus"
                            | "DOM.scrollIntoViewIfNeeded"
                            | "Target.createTarget"
                            | "Target.closeTarget"
                            | "Page.handleJavaScriptDialog"
                    ))
            {
                effects.store(true, Ordering::Release);
            }
            if dialog_open && dialog_blocks {
                return Err(Failure::unknown("browser_dialog_open"));
            }
        }
        // Timeout never releases the outstanding command's slot or implies it stopped.
        match tokio::time::timeout_at(deadline.into(), result).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Failure::unknown("browser_lost")),
            Err(_) => Err(Failure::unknown("browser_timeout_unknown")),
        }
    }
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
    /// Release only a press that may have reached the browser. Any failure after
    /// that point describes an uncertain input outcome, including failed cleanup.
    pub async fn input_pair(
        &self,
        session: Option<&str>,
        method: &str,
        press: Value,
        release: Value,
        deadline: Instant,
    ) -> Result<()> {
        let pressed = self.call(session, method, press, deadline).await;
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
                method,
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
        !self.inner.closed.load(Ordering::Acquire)
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
    pub fn stop(&self) {
        self.inner.finish();
    }
    pub fn authorized(&self, control: Arc<Control>, epoch: u64) -> Self {
        let mut copy = self.clone();
        copy.authorization = Some((control, epoch, false));
        copy
    }
    pub fn human(&self, control: Arc<Control>, epoch: u64) -> Self {
        let mut copy = self.clone();
        copy.authorization = Some((control, epoch, true));
        copy
    }
    pub fn track_effects(&self) -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let mut copy = self.clone();
        copy.effects = Some(flag.clone());
        (copy, flag)
    }
    pub fn cleanup(&self) -> Self {
        let mut copy = self.clone();
        copy.authorization = None;
        copy
    }
}

impl Inner {
    fn finish(&self) {
        let mut pending = self.pending.lock().unwrap();
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop.send_replace(true);
        for (_, command) in pending.commands.drain() {
            if let Some(reply) = command.reply {
                let _ = reply.send(Err(Failure::unknown("browser_lost")));
            }
        }
        pending.dialogs.clear();
    }
}
