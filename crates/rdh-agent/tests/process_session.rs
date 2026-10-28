//! Exercise native jobs through the production authenticated session and RPC.
#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
))]

use futures_util::{SinkExt, StreamExt};
use rdh_agent::{
    config::{DeviceConfig, local_roots},
    network::Network,
    policy::Policy,
    processes::PROCESS_OPERATIONS,
    random,
    rpc::Runtime,
    session,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};

const TOKEN: &str = "process-session-fixture-token";

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Peer {
    socket: WebSocketStream<TcpStream>,
    receipts: Vec<Value>,
    next: u64,
}
impl Peer {
    async fn connect(listener: &TcpListener) -> (Self, Value) {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/agent/process-session");
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer {TOKEN}")
            );
            Ok(response)
        })
        .await
        .unwrap();
        let mut peer = Self {
            socket,
            receipts: Vec::new(),
            next: 0,
        };
        let hello = peer.receive().await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["version"], 1);
        assert_eq!(hello["info"]["allow_shell"], true);
        let operations = hello["info"]["capabilities"]["operations"]
            .as_array()
            .unwrap();
        for operation in PROCESS_OPERATIONS {
            assert!(operations.contains(&json!(operation)));
        }
        peer.send(json!({"type":"ready","device_id":"process-session"}))
            .await;
        (peer, hello)
    }
    async fn send(&mut self, value: Value) {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
    async fn receive(&mut self) -> Value {
        loop {
            match self.socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => self.socket.send(Message::Pong(data)).await.unwrap(),
                Message::Pong(_) => {}
                other => panic!("unexpected session message {other:?}"),
            }
        }
    }
    async fn call(&mut self, operation: &str, params: Value, receipt: Option<Value>) -> Value {
        self.next += 1;
        let id = format!("process-request-{}", self.next);
        let mut request = json!({"type":"request","id":id,"operation":operation,"params":params});
        if let Some(receipt) = receipt {
            request["operation_receipt"] = receipt;
        }
        self.send(request).await;
        loop {
            let response = self.receive().await;
            match response["type"].as_str() {
                Some("result") => {
                    assert_eq!(response["id"], id);
                    assert!(response.get("error").is_none(), "{operation}: {response}");
                    return response["result"].clone();
                }
                Some("operation_result") => self.receipts.push(response),
                Some("heartbeat") => {}
                _ => panic!("unexpected RPC response {response}"),
            }
        }
    }
    async fn receipt(&mut self) -> Value {
        if let Some(receipt) = self.receipts.pop() {
            return receipt;
        }
        loop {
            let response = self.receive().await;
            if response["type"] == "heartbeat" {
                continue;
            }
            assert_eq!(response["type"], "operation_result");
            return response;
        }
    }
    async fn done(&mut self, id: &Value) -> Value {
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(if cfg!(windows) { 65 } else { 10 });
        loop {
            let result = self.call("read_process", json!({"job_id":id}), None).await;
            if result["status"] != "running" && result["cleanup_pending"] == false {
                return result;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "process did not finish: {result}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn echo_command() -> &'static str {
    if cfg!(windows) {
        "[Console]::OpenStandardInput().CopyTo([Console]::OpenStandardOutput()); exit 7"
    } else {
        "cat; exit 7"
    }
}
fn wait_command() -> &'static str {
    if cfg!(windows) {
        "Start-Sleep -Seconds 60"
    } else {
        "sleep 60"
    }
}

async fn exercise(listener: TcpListener) {
    let (mut peer, hello) = Peer::connect(&listener).await;
    let instance = hello["info"]["capabilities"]["operation_receipts"]["instance_id"].clone();
    let receipt_id = json!({"operation_id":"e".repeat(32),"instance_id":instance});
    let started = peer
        .call(
            "start_process",
            json!({"root":"work","command":echo_command(),"wait_seconds":0}),
            Some(receipt_id.clone()),
        )
        .await;
    assert_eq!(started["status"], "running");
    let job = started["job_id"].clone();
    let receipt = peer.receipt().await;
    assert_eq!(receipt["execution_state"], "succeeded");
    let listed = peer.call("list_processes", json!({}), None).await;
    assert!(
        listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["job_id"] == job)
    );

    // Keep the running input reader across a dropped connection. Replaying its
    // unacknowledged start receipt must preserve the original job identity.
    drop(peer);
    let (mut peer, hello) = Peer::connect(&listener).await;
    assert_eq!(
        hello["info"]["capabilities"]["operation_receipts"]["instance_id"],
        instance
    );
    assert_eq!(peer.receipt().await, receipt);
    peer.send(json!({"type":"operation_ack","operation_id":receipt_id["operation_id"],"instance_id":instance})).await;
    let status = peer.call("get_operation_status", receipt_id, None).await;
    assert_eq!(status["execution_state"], "succeeded");
    let input = peer
        .call(
            "write_process_stdin",
            json!({"job_id":job,"text":"go😀\n","close":true}),
            None,
        )
        .await;
    assert_eq!(input["sent"], 4);
    assert_eq!(input["stdin_closed"], true);
    let completed = peer.done(&job).await;
    assert_eq!(completed["status"], "exited");
    assert_eq!(completed["exit_code"], 7);
    assert_eq!(completed["output"], "go😀\n");

    let waiting = peer
        .call(
            "start_process",
            json!({"root":"work","command":wait_command(),"wait_seconds":0}),
            None,
        )
        .await;
    let id = &waiting["job_id"];
    peer.call("terminate_process", json!({"job_id":id}), None)
        .await;
    assert_eq!(peer.done(id).await["status"], "terminated");
    // Leave another child live so Runtime::shutdown must own and drain it.
    let shutdown = peer
        .call(
            "start_process",
            json!({"root":"work","command":wait_command(),"wait_seconds":0}),
            None,
        )
        .await;
    assert_eq!(shutdown["status"], "running");
}

#[tokio::test]
async fn process_rpc_receipts_reconnect_stdin_termination_and_shutdown() {
    let fixture = Temporary(std::env::temp_dir().join(format!(
        "rdh-process-session-{}",
        random::hex_token::<12>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = DeviceConfig {
        gateway_url: format!("http://{}", listener.local_addr().unwrap()),
        device_id: "process-session".into(),
        token: TOKEN.into(),
        roots: local_roots(&[format!("work={}", fixture.0.display())]).unwrap(),
        allow_shell: true,
        allow_write: false,
        shell: if cfg!(windows) {
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
                .to_str()
                .unwrap()
                .into()
        } else {
            "/bin/sh".into()
        },
        max_processes: 1,
        max_process_seconds: 60,
    };
    let runtime = Runtime::new(config.clone(), Policy::default(), None).unwrap();
    let network = Network::new(None, None, "", None).unwrap();
    let (stop, shutdown) = watch::channel(false);
    let running = Arc::clone(&runtime);
    let mut agent =
        tokio::spawn(
            async move { session::run(&network, &config, &running, shutdown, None).await },
        );
    let mut hub = tokio::spawn(exercise(listener));
    let result = tokio::time::timeout(
        Duration::from_secs(if cfg!(windows) { 90 } else { 40 }),
        &mut hub,
    )
    .await;
    if result.is_err() {
        hub.abort();
        let _ = hub.await;
    }
    let _ = stop.send(true);
    let stopped = tokio::time::timeout(Duration::from_secs(5), &mut agent).await;
    if stopped.is_err() {
        agent.abort();
        let _ = agent.await;
    }
    let drained = runtime.shutdown().await;
    drop(runtime);
    result
        .expect("process session timed out")
        .expect("Hub fixture failed");
    stopped
        .expect("Agent session did not stop")
        .unwrap()
        .unwrap();
    assert!(
        drained,
        "native process owners did not finish during shutdown"
    );
    fs::remove_dir_all(&fixture.0).unwrap();
}
