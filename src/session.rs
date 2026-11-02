//! Protocol v1 connection lifecycle for the development Agent.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{sync::watch, time::Instant};
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

const RECOVERY_BUDGET: Duration = Duration::from_secs(10);

struct SessionState {
    failed_assignment: Option<String>,
    discovery_attempt: Option<String>,
    previous_connection: Option<crate::cluster::Identity>,
    recovery_started: Option<Instant>,
    process_started: Instant,
    core_attempted: bool,
    fast_core_attempts: u8,
}

impl SessionState {
    fn new() -> Self {
        Self {
            failed_assignment: None,
            discovery_attempt: None,
            previous_connection: None,
            recovery_started: None,
            process_started: Instant::now(),
            core_attempted: false,
            fast_core_attempts: 0,
        }
    }

    fn fast_deadline(&self) -> Option<Instant> {
        self.recovery_started
            .map(|started| started + RECOVERY_BUDGET)
            .filter(|deadline| *deadline > Instant::now())
    }

    fn report(&self, event: &str, extra: Value) {
        eprintln!(
            "Connection lifecycle: {}",
            json!({
                "event":event,"monotonic_ms":self.process_started.elapsed().as_millis(),
                "connection":self.previous_connection,"detail":extra,
            })
        );
    }
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
    state: &mut SessionState,
) -> End {
    let _ready_session = ReadySession(readiness);
    state.core_attempted = false;
    let assignment = if let Some(discovery) = &config.discovery_url {
        if state.discovery_attempt.is_none() {
            match random::hex_token::<16>() {
                Ok(id) => state.discovery_attempt = Some(id),
                Err(_) => return End::Fatal("Unable to create discovery attempt".into()),
            }
        }
        let request = crate::cluster::AssignmentRequest {
            device_id: &config.device_id,
            instance_id: &runtime.receipts.instance_id,
            attempt_id: state.discovery_attempt.as_deref().unwrap(),
            failed_assignment_id: state.failed_assignment.as_deref(),
            previous_connection: state.previous_connection.as_ref(),
        };
        let deadline = state
            .fast_deadline()
            .map_or(Instant::now() + Duration::from_secs(20), |deadline| {
                deadline.min(Instant::now() + Duration::from_secs(2))
            });
        let assigned = tokio::select! {
            value=tokio::time::timeout_at(deadline, network.assign(discovery,&config.token,&request))=>value.unwrap_or_else(|_| Err(WebSocketError::Network("Cluster discovery timed out".into()))),
            _=interrupted(shutdown)=>return End::Stop,
        };
        match assigned {
            Ok(assignment) => {
                state.discovery_attempt = None;
                state.failed_assignment = Some(assignment.assignment_id.clone());
                Some(assignment)
            }
            Err(WebSocketError::Unauthorized(status)) => {
                return End::Fatal(format!(
                    "Cluster discovery authorization rejected (HTTP {status})"
                ));
            }
            Err(WebSocketError::Network(reason)) => {
                return End::Retry {
                    was_ready: false,
                    reason,
                };
            }
        }
    } else {
        None
    };
    let target = if let Some(assignment) = &assignment {
        match assignment.target(config.discovery_url.as_deref().unwrap()) {
            Ok(target) => target,
            Err(_) => return End::Fatal("Cluster returned an invalid connection target".into()),
        }
    } else {
        config.gateway_url.clone()
    };
    let credential = assignment
        .as_ref()
        .map_or(config.token.as_str(), |assignment| {
            assignment.access_ticket.as_str()
        });
    state.core_attempted = true;
    let core_deadline = state.fast_deadline().map(|deadline| {
        state.fast_core_attempts = state.fast_core_attempts.saturating_add(1);
        // The original Core gets one short attempt. Reserve the rest of the
        // same recovery budget for rediscovery and the successor's full handshake.
        let seconds = if state.fast_core_attempts == 1 { 2 } else { 4 };
        deadline.min(Instant::now() + Duration::from_secs(seconds))
    });
    let connect = async {
        let connect = network.websocket_at(
            &target,
            assignment.as_ref().and_then(|a| a.connect_ip),
            &config.device_id,
            credential,
        );
        match core_deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, connect)
                .await
                .unwrap_or_else(|_| {
                    Err(WebSocketError::Network("Core connection timed out".into()))
                }),
            None => connect.await,
        }
    };
    let socket = tokio::select! {
        value = connect => match value {
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
    let mut hello = json!({"type":"hello", "version":1, "info":information});
    if let Some(assignment) = &assignment {
        hello["cluster"] = json!({"version":1,"assignment_id":assignment.assignment_id,"node_id":assignment.node_id,"boot_id":assignment.boot_id});
    }
    if sender.json(&hello).is_err() {
        writer.abort();
        return End::Retry {
            was_ready: false,
            reason: "Gateway send failed".into(),
        };
    }
    let ready_deadline = tokio::time::sleep_until(
        core_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(10)),
    );
    tokio::pin!(ready_deadline);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(20),
        Duration::from_secs(20),
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ready = false;
    let mut accepted: Option<crate::cluster::Identity> = None;
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
                        if frame.as_ref().is_some_and(|frame| frame.code == CloseCode::Policy) {
                            break End::Fatal("Gateway authorization was revoked or the connection was rejected".into());
                        }
                        if !ready && frame.as_ref().is_some_and(|frame| {
                            frame.code == CloseCode::Again
                                && matches!(frame.reason.as_ref(), "device_already_connected" | "cluster_connection_stale" | "assignment_expired")
                        }) {
                            // A valid Core waiting for ownership is not an
                            // unreachable target; do not put it into cooldown.
                            state.failed_assignment = None;
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
                    if let Some(assignment)=&assignment {
                        let identity=value.get("cluster").cloned().and_then(|v|serde_json::from_value::<crate::cluster::Identity>(v).ok());
                        let Some(identity)=identity.filter(|id|assignment.matches(id,&config.device_id,&runtime.receipts.instance_id)) else {
                            break End::Retry{was_ready:false,reason:"Invalid cluster connection identity".into()};
                        };
                        if let Some(current)=&accepted {
                            if value["type"]!="active" || current!=&identity {
                                break End::Retry{was_ready:false,reason:"Cluster did not confirm activation".into()};
                            }
                        } else {
                            if value["type"]!="ready" || !runtime.bind_cluster(generation,identity.clone(),sender.clone()) {
                                break End::Retry{was_ready:false,reason:"Cluster control generation was rejected".into()};
                            }
                            accepted=Some(identity);
                            continue;
                        }
                    } else {
                    if value.get("type").and_then(Value::as_str) != Some("ready") {
                        break End::Retry { was_ready: false, reason: "Gateway did not send ready".into() };
                    }
                    if !runtime.bind(generation, sender.clone()) {
                        break End::Fatal("Agent could not bind the ready connection".into());
                    }
                    }
                    ready = true;
                    state.failed_assignment=None;
                    state.previous_connection=runtime.last_cluster_identity();
                    state.report("ready", json!({"recovery_elapsed_ms":state.recovery_started.map(|started|started.elapsed().as_millis())}));
                    state.recovery_started=None;
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
    state.previous_connection = runtime.last_cluster_identity();
    if matches!(
        outcome,
        End::Retry {
            was_ready: true,
            ..
        }
    ) {
        state.recovery_started = Some(Instant::now());
        state.fast_core_attempts = 0;
        state.report("disconnected", json!({}));
    }
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
    let mut state = SessionState::new();
    loop {
        let outcome = session(
            network,
            config,
            runtime,
            &mut shutdown,
            readiness,
            &mut state,
        )
        .await;
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
                let immediate = state.fast_deadline().is_some()
                    && (was_ready
                        || state.core_attempted
                            && state.failed_assignment.is_some()
                            && state.fast_core_attempts < 2);
                let wait = if immediate { 0 } else { delay };
                eprintln!("{reason}; retrying in {:.1}s", wait as f64 / 1000.0);
                let jitter = u64::from(u16::from_ne_bytes(random::bytes::<2>()?))
                    % if immediate { 200 } else { 1000 };
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(wait + jitter)) => {},
                    _ = interrupted(&mut shutdown) => return Ok(()),
                }
                if !immediate {
                    delay = (delay * 2).min(30_000);
                }
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod admission_tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::post};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    const TICKET: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-ABCDE";
    const LONG_TOKEN: &str = "long-lived-device-token-must-only-reach-the-gate";

    #[tokio::test]
    async fn discovery_binds_instance_and_core_receives_only_the_ticket() {
        let gate = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let core = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let core_address = core.local_addr().unwrap();
        let config = DeviceConfig {
            gateway_url: "http://127.0.0.1:1".into(),
            discovery_url: Some(format!("http://{}", gate.local_addr().unwrap())),
            device_id: "admission-test".into(),
            token: LONG_TOKEN.into(),
            roots: crate::config::local_roots(&[format!(
                "work={}",
                std::env::temp_dir().display()
            )])
            .unwrap(),
            allow_write: false,
            allow_shell: false,
            shell: "/bin/sh".into(),
            max_processes: 1,
            max_process_seconds: 10,
        };
        let policy = crate::policy::Policy::parse(&json!({"version":1}), &config).unwrap();
        let runtime = Runtime::new(config.clone(), policy, None).unwrap();
        let expected_instance = runtime.receipts.instance_id.clone();
        let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
        let returned_assignment = Arc::new(Mutex::new(
            json!({"version":2,"assignment_id":"assignment","node_id":"node","boot_id":"boot","agent_url":format!("ws://{core_address}"),"expires_at":4102444800.0,"access_ticket":TICKET}),
        ));
        let gate_assignment = Arc::clone(&returned_assignment);
        let app = Router::new().route(
            "/agent/assign",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let observed = observed.clone();
                let assignment = gate_assignment.lock().unwrap().clone();
                async move {
                    observed.send((headers, body)).unwrap();
                    Json(assignment)
                }
            }),
        );
        let gate_task = tokio::spawn(async move { axum::serve(gate, app).await.unwrap() });
        let core_task = tokio::spawn(async move {
            let (stream, _) = core.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &Request, response: Response| {
                    assert_eq!(
                        request.headers()["authorization"],
                        format!("Bearer {TICKET}")
                    );
                    assert!(!format!("{request:?}").contains(LONG_TOKEN));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let hello = socket.next().await.unwrap().unwrap();
            let hello: Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
            assert_eq!(hello["version"], 1);
            assert_eq!(hello["cluster"]["version"], 1);
            assert!(hello["cluster"].get("access_ticket").is_none());
            socket
                .send(Message::Close(Some(
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: CloseCode::Policy,
                        reason: "fixture complete".into(),
                    },
                )))
                .await
                .unwrap();
        });
        let network = Network::new(None, None, "", None).unwrap();
        let (_stop, mut shutdown) = watch::channel(false);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            session(
                &network,
                &config,
                &runtime,
                &mut shutdown,
                None,
                &mut SessionState::new(),
            ),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, End::Fatal(_)));
        let (headers, body) = observations.recv().await.unwrap();
        assert_eq!(headers["authorization"], format!("Bearer {LONG_TOKEN}"));
        assert_eq!(headers["x-rdh-agent-transport"], "direct-ip-v2");
        assert_eq!(body["instance_id"], expected_instance);
        assert_eq!(body["device_id"], "admission-test");
        assert_eq!(body["attempt_id"].as_str().unwrap().len(), 32);
        core_task.await.unwrap();
        // Invalid discovery replies must finish before any Core TCP connection,
        // without falling back to the configured long-lived bearer.
        let forbidden_core = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = returned_assignment.lock().unwrap().clone();
        for ticket in [None, Some(""), Some("malformed-ticket")] {
            let mut invalid = base.clone();
            invalid["agent_url"] = format!("ws://{}", forbidden_core.local_addr().unwrap()).into();
            match ticket {
                Some(ticket) => invalid["access_ticket"] = ticket.into(),
                None => {
                    invalid.as_object_mut().unwrap().remove("access_ticket");
                }
            }
            *returned_assignment.lock().unwrap() = invalid;
            let mut state = SessionState::new();
            tokio::select! {
                _ = forbidden_core.accept() => panic!("invalid ticket reached Core"),
                result = tokio::time::timeout(Duration::from_secs(5), session(&network, &config, &runtime, &mut shutdown, None, &mut state)) => {
                    assert!(matches!(result.unwrap(), End::Fatal(_) | End::Retry { .. }));
                }
            }
        }
        gate_task.abort();
    }
}
