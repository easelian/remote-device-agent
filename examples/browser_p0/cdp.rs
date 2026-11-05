//! Bounded experimental transport. Timeouts leave commands pending until reply/EOF.
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

use super::Result;

struct Pending {
    reply: oneshot::Sender<std::result::Result<Value, &'static str>>,
    _permit: OwnedSemaphorePermit,
}

struct Inner {
    pending: Mutex<HashMap<u64, Pending>>,
    slots: Arc<Semaphore>,
    next: AtomicU64,
    late: AtomicU64,
    closed: std::sync::atomic::AtomicBool,
    close_reason: Mutex<Option<&'static str>>,
}

#[derive(Clone)]
pub struct Cdp {
    inner: Arc<Inner>,
    outgoing: mpsc::Sender<String>,
    events: broadcast::Sender<Value>,
}

impl Cdp {
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(2 * 1024 * 1024))
            .max_frame_size(Some(2 * 1024 * 1024));
        let (mut socket, _) = connect_async_with_config(endpoint, Some(config), false).await?;
        let (outgoing, mut rx) = mpsc::channel::<String>(32);
        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            pending: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(32)),
            next: AtomicU64::new(1),
            late: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            close_reason: Mutex::new(None),
        });
        let actor = inner.clone();
        let notifications = events.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = rx.recv() => {
                        let Some(command) = command else { break };
                        match tokio::time::timeout(Duration::from_secs(3), socket.send(Message::Text(command.into()))).await {
                            Ok(Ok(())) => {},
                            _ => break,
                        }
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let Ok(value) = serde_json::from_str::<Value>(&text) else { break };
                                if let Some(id) = value["id"].as_u64() {
                                    if let Some(pending) = actor.pending.lock().unwrap().remove(&id) {
                                        // Never include raw protocol errors (possibly page content) in diagnostics.
                                        let result = if value.get("error").is_some() { Err("cdp_command_error") }
                                            else { Ok(value["result"].clone()) };
                                        if pending.reply.send(result).is_err() {
                                            actor.late.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                } else {
                                    let _ = notifications.send(value);
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                if socket.send(Message::Pong(data)).await.is_err() { break; }
                            }
                            Some(Ok(Message::Pong(_))) => {},
                            Some(Err(tokio_tungstenite::tungstenite::Error::Capacity(_))) => {
                                *actor.close_reason.lock().unwrap()=Some("message_capacity");
                                break;
                            },
                            _ => break,
                        }
                    }
                }
            }
            // Lock also serializes shutdown against a concurrent send.
            let mut pending = actor.pending.lock().unwrap();
            actor.closed.store(true, Ordering::Release);
            for (_, command) in pending.drain() {
                let _ = command.reply.send(Err("cdp_disconnected"));
            }
        });
        Ok(Self {
            inner,
            outgoing,
            events,
        })
    }

    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_for(session, method, params, Duration::from_secs(5))
            .await
    }

    pub async fn call_for(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let permit = self
            .inner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "cdp_busy")?;
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let encoded = serde_json::to_string(&message)?;
        if encoded.len() > 256 * 1024 {
            return Err("cdp_request_too_large".into());
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().unwrap();
            if self.inner.closed.load(Ordering::Acquire) {
                return Err("cdp_disconnected".into());
            }
            pending.insert(
                id,
                Pending {
                    reply: tx,
                    _permit: permit,
                },
            );
            if self.outgoing.try_send(encoded).is_err() {
                pending.remove(&id);
                return Err("cdp_not_sent".into());
            }
        }
        // Dropping rx must NOT remove the outstanding command or release its admission slot.
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Err(_)) => Err("cdp_disconnected".into()),
            Err(_) => Err("cdp_timeout_unknown".into()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
    pub fn pending(&self) -> usize {
        self.inner.pending.lock().unwrap().len()
    }
    pub fn late_replies(&self) -> u64 {
        self.inner.late.load(Ordering::Relaxed)
    }
    pub fn connected(&self) -> bool {
        !self.inner.closed.load(Ordering::Acquire)
    }

    pub fn close_reason(&self) -> Option<&'static str> {
        *self.inner.close_reason.lock().unwrap()
    }

    pub async fn wait_idle(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.pending() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| "cdp_still_pending")?;
        if !self.connected() {
            return Err("cdp_disconnected_not_a_handoff_barrier".into());
        }
        Ok(())
    }
}

pub async fn evaluate(cdp: &Cdp, session: &str, expression: &str) -> Result<Value> {
    let result = cdp
        .call(
            Some(session),
            "Runtime.evaluate",
            json!({
                "expression":expression,"returnByValue":true,"awaitPromise":true
            }),
        )
        .await?;
    if result.get("exceptionDetails").is_some() {
        return Err("page_evaluation_failed".into());
    }
    Ok(result["result"]["value"].clone())
}
