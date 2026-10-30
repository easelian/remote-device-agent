//! Exercise file backends through the production session and RPC runtime.
//! This fixture does not require pairing, private storage, or process support.
#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files"),
    all(windows, feature = "experimental-windows-files")
))]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rdh_agent::{
    config::{DeviceConfig, local_roots, sha256_hex},
    files::FILE_OPERATIONS,
    network::Network,
    policy::Policy,
    random,
    rpc::Runtime,
    searches::SEARCH_OPERATIONS,
    session,
};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
};
use tokio_tungstenite::{WebSocketStream, accept_hdr_async};

const TOKEN: &str = "file-session-fixture-token";
const CONTENT: &str = "needle 😀\n";

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
            assert_eq!(request.uri().path(), "/agent/file-session");
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
        let operations = hello["info"]["capabilities"]["operations"]
            .as_array()
            .unwrap();
        for operation in FILE_OPERATIONS.iter().chain(SEARCH_OPERATIONS) {
            assert!(
                operations.contains(&json!(operation)),
                "{operation}: {hello}"
            );
        }
        assert_eq!(hello["info"]["allow_shell"], false);
        assert_eq!(hello["info"]["allow_write"], true);
        peer.send(json!({"type":"ready","device_id":"file-session"}))
            .await;
        (peer, hello)
    }

    async fn send(&mut self, message: Value) {
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
    }

    async fn receive(&mut self) -> Value {
        loop {
            match self.socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => self.socket.send(Message::Pong(data)).await.unwrap(),
                Message::Pong(_) => {}
                other => panic!("unexpected fixture message: {other:?}"),
            }
        }
    }

    async fn result(&mut self, operation: &str, params: Value, receipt: Option<Value>) -> Value {
        self.next += 1;
        let id = format!("file-request-{}", self.next);
        let mut request = json!({"type":"request","id":id,"operation":operation,"params":params});
        if let Some(receipt) = receipt {
            request["operation_receipt"] = receipt;
        }
        self.send(request).await;
        loop {
            let value = self.receive().await;
            match value["type"].as_str() {
                Some("result") => {
                    assert_eq!(value["id"], id);
                    return value;
                }
                Some("operation_result") => self.receipts.push(value),
                Some("heartbeat") => {}
                _ => panic!("unexpected RPC response: {value}"),
            }
        }
    }

    async fn call(&mut self, operation: &str, params: Value) -> Value {
        let value = self.result(operation, params, None).await;
        assert!(value.get("error").is_none(), "{operation}: {value}");
        value["result"].clone()
    }

    async fn receipt(&mut self) -> Value {
        if let Some(receipt) = self.receipts.pop() {
            return receipt;
        }
        loop {
            let value = self.receive().await;
            if value["type"] == "heartbeat" {
                continue;
            }
            assert_eq!(value["type"], "operation_result");
            return value;
        }
    }
}

async fn exercise(listener: TcpListener) {
    let (mut peer, hello) = Peer::connect(&listener).await;
    let instance = hello["info"]["capabilities"]["operation_receipts"]["instance_id"].clone();
    assert!(instance.is_string());
    let operation_id = "c".repeat(32);
    let receipt_id = json!({"operation_id":operation_id,"instance_id":instance});
    let written = peer
        .result(
            "write_file",
            json!({"root":"work","path":"Straße.txt","content":CONTENT}),
            Some(receipt_id.clone()),
        )
        .await;
    assert!(written.get("error").is_none(), "{written}");
    assert_eq!(written["result"]["sha256"], sha256_hex(CONTENT.as_bytes()));
    let receipt = peer.receipt().await;
    assert_eq!(receipt["execution_state"], "succeeded");
    assert_eq!(receipt["operation_id"], operation_id);
    assert_eq!(
        receipt["result_summary"]["sha256"],
        sha256_hex(CONTENT.as_bytes())
    );

    // Lose the acknowledgement, then require the new connection to replay the
    // original outcome without sending the mutation a second time.
    drop(peer);
    let (mut peer, reconnected) = Peer::connect(&listener).await;
    assert_eq!(
        reconnected["info"]["capabilities"]["operation_receipts"]["instance_id"],
        instance
    );
    assert_eq!(peer.receipt().await, receipt);
    peer.send(json!({"type":"operation_ack","operation_id":operation_id,"instance_id":instance}))
        .await;
    let status = peer.call("get_operation_status", receipt_id).await;
    assert_eq!(status["execution_state"], "succeeded");

    let read = peer
        .call("read_file", json!({"root":"work","path":"Straße.txt"}))
        .await;
    assert_eq!(read["content"], CONTENT);
    assert_eq!(read["sha256"], written["result"]["sha256"]);
    let info = peer
        .call("file_info", json!({"root":"work","path":"Straße.txt"}))
        .await;
    assert_eq!(info["type"], "file");
    assert_eq!(info["size_bytes"], CONTENT.len());
    let chunk = peer
        .call(
            "read_file_chunk",
            json!({"root":"work","path":"Straße.txt"}),
        )
        .await;
    assert_eq!(chunk["content"], CONTENT);
    assert_eq!(chunk["fingerprint"], info["fingerprint"]);
    let batch = peer
        .call(
            "read_multiple_files",
            json!({"root":"work","paths":["Straße.txt","absent"]}),
        )
        .await;
    assert_eq!(batch["files"][0]["content"], CONTENT);
    assert!(batch["files"][1]["error"].is_string());

    let conflict = peer.result("write_file", json!({"root":"work","path":"Straße.txt","content":"wrong","expected_sha256":"0".repeat(64)}), None).await;
    assert!(
        conflict["error"]
            .as_str()
            .unwrap()
            .contains("Content conflict")
    );
    let updated = peer.call("edit_file", json!({"root":"work","path":"Straße.txt","old_text":"needle","new_text":"updated","expected_sha256":read["sha256"]})).await;
    assert_eq!(updated["sha256"], sha256_hex("updated 😀\n".as_bytes()));
    peer.call("make_directory", json!({"root":"work","path":"nested"}))
        .await;
    peer.call(
        "move_file",
        json!({"root":"work","source":"Straße.txt","destination":"nested/Straße.txt"}),
    )
    .await;
    let list = peer
        .call("list_directory", json!({"root":"work","path":"nested"}))
        .await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["entries"][0]["name"], "Straße.txt");
    let search = peer
        .call("search_files", json!({"root":"work","query":"strasse"}))
        .await;
    assert_eq!(search["matches"], json!([{"path":"nested/Straße.txt"}]));

    let started = peer
        .call(
            "start_search",
            json!({"root":"work","query":"updated","content":true}),
        )
        .await;
    let id = &started["search_id"];
    assert!(id.is_string());
    let search = loop {
        let result = peer
            .call("get_search_results", json!({"search_id":id}))
            .await;
        if !matches!(result["status"].as_str(), Some("queued" | "running")) {
            break result;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(search["status"], "completed");
    assert_eq!(
        search["results"],
        json!([{"path":"nested/Straße.txt","line":1,"text":"updated 😀"}])
    );
    let retained = peer.call("list_searches", json!({})).await;
    assert!(
        retained["searches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|search| &search["search_id"] == id)
    );
    let stopped = peer.call("stop_search", json!({"search_id":id})).await;
    assert_eq!(stopped["status"], "completed");
    let protected = peer
        .result(
            "write_file",
            json!({"root":"work","path":".env","content":"must not be written"}),
            None,
        )
        .await;
    assert_eq!(protected["error_code"], "policy_path_protected");
    assert_eq!(protected["execution_state"], "not_sent");
    let traversal = peer
        .result(
            "read_file",
            json!({"root":"work","path":"../outside"}),
            None,
        )
        .await;
    assert!(traversal["error"].is_string());
}

#[tokio::test]
async fn files_and_searches_survive_reconnection_and_release_their_owners() {
    let fixture = Temporary(std::env::temp_dir().join(format!(
        "rdh-file-session-{}",
        random::hex_token::<12>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = DeviceConfig {
        gateway_url: format!("http://{}", listener.local_addr().unwrap()),
        device_id: "file-session".into(),
        token: TOKEN.into(),
        roots: local_roots(&[format!("work={}", fixture.0.display())]).unwrap(),
        allow_write: true,
        allow_shell: false,
        shell: if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into(),
        max_processes: 1,
        max_process_seconds: 10,
    };
    let policy = Policy::parse(&json!({"version":1,"protected_paths":[".env"]}), &config).unwrap();
    let runtime = Runtime::new(config.clone(), policy, None).unwrap();
    let network = Network::new(None, None, "", None).unwrap();
    let (stop, shutdown) = watch::channel(false);
    let running = std::sync::Arc::clone(&runtime);
    let mut agent =
        tokio::spawn(
            async move { session::run(&network, &config, &running, shutdown, None).await },
        );
    let mut hub = tokio::spawn(exercise(listener));
    let result = tokio::time::timeout(Duration::from_secs(30), &mut hub).await;
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
    // Assert results only after stopping both owners, including failed fixtures.
    result
        .expect("file session timed out")
        .expect("Hub fixture failed");
    stopped
        .expect("Agent session did not stop")
        .unwrap()
        .unwrap();
    assert!(
        drained,
        "file/search operations did not finish during shutdown"
    );
    assert_eq!(
        fs::read(fixture.0.join("nested/Straße.txt")).unwrap(),
        "updated 😀\n".as_bytes()
    );
    assert!(!fixture.0.join("Straße.txt").exists());
    assert!(!fixture.0.join(".env").exists());
    // On Windows this also checks that no directory handle still blocks removal.
    fs::remove_dir_all(&fixture.0).unwrap();
}
