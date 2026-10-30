//! Protocol v1 connection lifecycle for the development Agent.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{Message, protocol::frame::coding::CloseCode};

use crate::config::DeviceConfig;
use crate::error::{Error, Result};
use crate::network::{Network, WebSocketError};
use crate::readiness::Reporter;
use crate::rpc::Runtime;
use crate::{random, transport};

async fn interrupted(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

enum End {
    Stop,
    Retry { was_ready: bool, reason: String },
    Fatal(String),
}

struct ReadySession<'a>(Option<&'a Reporter>);
impl Drop for ReadySession<'_> {
    fn drop(&mut self) {
        if let Some(reporter) = self.0 {
            reporter.offline();
        }
    }
}

async fn session(
    network: &Network,
    config: &DeviceConfig,
    runtime: &Arc<Runtime>,
    shutdown: &mut watch::Receiver<bool>,
    readiness: Option<&Reporter>,
) -> End {
    let _ready_session = ReadySession(readiness);
    let socket = tokio::select! {
        value = network.websocket(&config.gateway_url, &config.device_id, &config.token) => match value {
            Ok(socket) => socket,
            Err(WebSocketError::Unauthorized(status)) => return End::Fatal(format!("Gateway authorization rejected (HTTP {status})")),
            Err(WebSocketError::Network(reason)) => return End::Retry { was_ready: false, reason },
        },
        _ = interrupted(shutdown) => return End::Stop,
    };
    let (mut sink, mut stream) = socket.split();
    let (sender, mut rx, mut send_failed) = transport::channel();
    let generation = runtime.next_generation();
    let active_ids = Arc::new(Mutex::new(HashSet::new()));
    let mut writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if tokio::time::timeout(Duration::from_secs(5), sink.send(frame.message))
                .await
                .map_err(|_| ())?
                .is_err()
            {
                return Err(());
            }
        }
        Ok::<(), ()>(())
    });
    let information = runtime.info();
    if serde_json::to_vec(&information).map_or(true, |bytes| bytes.len() > 16 * 1024) {
        writer.abort();
        return End::Fatal("Device information exceeds the Hub hello limit".into());
    }
    if sender
        .json(&json!({"type":"hello", "version":1, "info":information}))
        .is_err()
    {
        writer.abort();
        return End::Retry {
            was_ready: false,
            reason: "Gateway send failed".into(),
        };
    }
    let ready_deadline = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(ready_deadline);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(20),
        Duration::from_secs(20),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ready = false;
    let mut alive = true;
    let mut writer_finished = false;
    let outcome = loop {
        tokio::select! {
            _ = interrupted(shutdown) => break End::Stop,
            _ = &mut ready_deadline, if !ready => break End::Retry { was_ready: false, reason: "Gateway ready timed out".into() },
            _ = &mut writer => {
                writer_finished = true;
                break End::Retry { was_ready: ready, reason: "Gateway send failed".into() };
            },
            _ = send_failed.changed() => break End::Retry { was_ready: ready, reason: "Gateway send budget exceeded".into() },
            _ = heartbeat.tick(), if ready => {
                if !alive { break End::Retry { was_ready: true, reason: "Gateway heartbeat timed out".into() }; }
                alive = false;
                if sender.send(Message::Ping(Vec::new().into())).is_err()
                    || sender.json(&json!({"type":"heartbeat"})).is_err() {
                    break End::Retry { was_ready: true, reason: "Gateway send budget exceeded".into() };
                }
            }
            message = stream.next() => {
                let raw = match message {
                    Some(Ok(Message::Text(value))) => value.as_bytes().to_vec(),
                    Some(Ok(Message::Binary(value))) => value.to_vec(),
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).is_err() {
                            break End::Retry { was_ready: ready, reason: "Gateway send budget exceeded".into() };
                        }
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => { alive = true; continue; }
                    Some(Ok(Message::Close(frame))) => {
                        if frame.is_some_and(|frame| frame.code == CloseCode::Policy) {
                            break End::Fatal("Gateway authorization was revoked or the connection was rejected".into());
                        }
                        break End::Retry { was_ready: ready, reason: "Gateway disconnected".into() };
                    }
                    Some(Ok(_)) => continue,
                    _ => break End::Retry { was_ready: ready, reason: "Gateway disconnected".into() },
                };
                let value = match serde_json::from_slice::<Value>(&raw) {
                    Ok(Value::Object(value)) => Value::Object(value),
                    _ => break End::Retry { was_ready: ready, reason: "Invalid Gateway message".into() },
                };
                if !ready {
                    if value.get("type").and_then(Value::as_str) != Some("ready") {
                        break End::Retry { was_ready: false, reason: "Gateway did not send ready".into() };
                    }
                    if !runtime.bind(generation, sender.clone()) {
                        break End::Fatal("Agent could not bind the ready connection".into());
                    }
                    ready = true;
                    if let Some(reporter) = readiness { reporter.ready(generation); }
                    println!("Device {} connected", config.device_id);
                    heartbeat.reset();
                    continue;
                }
                match value.get("type").and_then(Value::as_str) {
                    Some("heartbeat") => {},
                    Some("operation_ack") => runtime.acknowledge(&value),
                    Some("request") => if runtime.receive(&value, &sender, &active_ids).is_err() {
                        break End::Retry { was_ready: ready, reason: "Invalid request or send budget exceeded".into() };
                    },
                    _ => break End::Retry { was_ready: ready, reason: "Invalid Gateway message".into() },
                }
            }
        }
    };
    if let Some(reporter) = readiness {
        reporter.offline();
    }
    runtime.unbind(generation);
    drop(sender);
    if !writer_finished {
        writer.abort();
        let _ = writer.await;
    }
    outcome
}

pub async fn run(
    network: &Network,
    config: &DeviceConfig,
    runtime: &Arc<Runtime>,
    mut shutdown: watch::Receiver<bool>,
    readiness: Option<&Reporter>,
) -> Result<()> {
    let mut delay = 1_000_u64;
    loop {
        let outcome = session(network, config, runtime, &mut shutdown, readiness).await;
        match outcome {
            End::Stop => return Ok(()),
            End::Fatal(message) => return Err(Error::message(message)),
            End::Retry { was_ready, reason } => {
                if was_ready {
                    delay = 1_000;
                }
                if *shutdown.borrow() {
                    return Ok(());
                }
                eprintln!("{reason}; retrying in {:.1}s", delay as f64 / 1000.0);
                let jitter = u64::from(u16::from_ne_bytes(random::bytes::<2>()?)) % 1000;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay + jitter)) => {},
                    _ = interrupted(&mut shutdown) => return Ok(()),
                }
                delay = (delay * 2).min(30_000);
            }
        }
    }
}
