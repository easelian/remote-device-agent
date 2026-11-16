//! Private Chromium pipe, with bounded framing, queueing and late-reply ownership.
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
mod submission_tests;
use command::Dialog;
pub use command::{Command, InputPair};

#[cfg(test)]
mod metrics;
const MAX_MESSAGE: usize = 2 * 1024 * 1024;
struct Pending {
    #[cfg(test)]
    metric: metrics::Command,
    reply: Option<oneshot::Sender<Result<Value>>>,
    session: Option<String>,
    dialog_blocks: bool,
    input_barrier: bool,
    _permit: OwnedSemaphorePermit,
}
#[derive(Default)]
struct PendingState {
    #[cfg(test)]
    resources: metrics::Resources,
    commands: HashMap<u64, Pending>,
    dialogs: HashSet<String>,
}
struct Inner {
    pending: Mutex<PendingState>,
    next: AtomicU64,
    closed: AtomicBool,
    slots: Arc<Semaphore>,
    stop: watch::Sender<Shutdown>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shutdown {
    Running,
    Graceful,
    Immediate,
}

#[derive(Clone)]
pub struct Cdp {
    inner: Arc<Inner>,
    outgoing: mpsc::Sender<Vec<u8>>,
    events: broadcast::Sender<Value>,
    submission: Submission,
}

impl Cdp {
    #[cfg(test)]
    pub fn test_metrics(&self) -> Value {
        let pending = self.inner.pending.lock().unwrap();
        json!({"commands":self.inner.next.load(Ordering::Relaxed)-1,"pending":pending.commands.len(),"native_objects":pending.resources.objects.len(),"streams":pending.resources.streams.len()})
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
        let (outgoing, mut requests) = mpsc::channel::<Vec<u8>>(32);
        let (events, _) = broadcast::channel(256);
        let (stop, mut stopped) = watch::channel(Shutdown::Running);
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
                        frame.clear();
                        if let Some(id) = value["id"].as_u64() {
                            let command = {
                                let mut pending = reader.pending.lock().unwrap();
                                let command = pending.commands.remove(&id);
                                #[cfg(test)]
                                if let Some(command) = &command {
                                    pending.resources.settle(&command.metric, &value);
                                }
                                command
                            };
                            if let Some(command) = command {
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
                    biased;
                    _ = stopped.changed() => {
                        let graceful = *stopped.borrow_and_update() == Shutdown::Graceful;
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

    pub async fn call(
        &self,
        session: Option<&str>,
        command: Command,
        params: Value,
        deadline: Instant,
    ) -> Result<Value> {
        let spec = command.spec();
        if Instant::now() >= deadline {
            return Err(Failure::new("browser_deadline"));
        }
        if !self.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        let permit = self
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::new("browser_busy"))?;
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
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
        let blocked_release = self.submission.enqueue(spec.effect, || {
            let mut pending = self.inner.pending.lock().unwrap();
            if self.inner.closed.load(Ordering::Acquire) {
                return Err(Failure::unknown("browser_lost"));
            }
            let dialog_blocks = spec.dialog != Dialog::Continue;
            let dialog_open = session.is_some_and(|s| pending.dialogs.contains(s));
            if dialog_open && spec.dialog == Dialog::Block {
                return Err(Failure::new("browser_dialog_open"));
            }
            pending.commands.insert(
                id,
                Pending {
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
                pending.commands.remove(&id);
                return Err(Failure::new("browser_busy"));
            }
            Ok(dialog_open && dialog_blocks)
        })?;
        if blocked_release {
            return Err(Failure::unknown("browser_dialog_open"));
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
        for (_, command) in pending.commands.drain() {
            if let Some(reply) = command.reply {
                let _ = reply.send(Err(Failure::unknown("browser_lost")));
            }
        }
        pending.dialogs.clear();
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
