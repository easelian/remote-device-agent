//! Bounded serial protocol probe. Cancellation poisons it; D2b will supply the
//! real pending table and shared submission gate before any page input exists.
use super::{Result, members};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::VecDeque, fs, time::Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const MAX_MESSAGE: usize = 2 * 1024 * 1024;
const MAX_EVENTS: usize = 64;

#[derive(Clone)]
pub(super) struct Endpoint {
    port: u16,
}
impl Endpoint {
    fn parse(raw: &str) -> Result<Self> {
        let url = url::Url::parse(raw).map_err(|_| "invalid_endpoint")?;
        let port = url.port().filter(|p| *p != 0).ok_or("invalid_endpoint")?;
        if url.scheme() != "ws"
            || url.host_str() != Some("127.0.0.1")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || ![
                format!("ws://127.0.0.1:{port}"),
                format!("ws://127.0.0.1:{port}/session"),
            ]
            .contains(&raw.to_owned())
        {
            return Err("invalid_endpoint");
        }
        Ok(Self { port })
    }

    pub(super) fn announcement(bytes: &[u8]) -> Result<Option<Self>> {
        // stderr is observed while Firefox writes it. An incomplete line or
        // trailing UTF-8 character is not an invalid completed announcement.
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes[..=end]).map_err(|_| "invalid_startup_log")?;
        let mut found = None;
        for line in text.lines() {
            if let Some(raw) = line.strip_prefix("WebDriver BiDi listening on ") {
                if found.is_some() {
                    return Err("ambiguous_endpoint");
                }
                found = Some(Self::parse(raw)?);
            }
        }
        Ok(found)
    }

    pub(super) fn uri(&self) -> String {
        format!("ws://127.0.0.1:{}/session", self.port)
    }

    pub(super) fn owned_by(&self, group: u32) -> Result<bool> {
        let address = format!("0100007F:{:04X}", self.port);
        let table = fs::read_to_string("/proc/net/tcp").map_err(|_| "socket_table_unavailable")?;
        let mut inodes = Vec::new();
        for row in table.lines().skip(1) {
            let columns: Vec<_> = row.split_whitespace().collect();
            if columns.get(1) == Some(&address.as_str()) && columns.get(3) == Some(&"0A") {
                inodes.push(format!(
                    "socket:[{}]",
                    columns.get(9).ok_or("socket_table_invalid")?
                ));
            }
        }
        for pid in members(group)? {
            let directory = match fs::read_dir(format!("/proc/{pid}/fd")) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err("socket_owner_unavailable"),
            };
            for entry in directory {
                let entry = entry.map_err(|_| "socket_owner_unavailable")?;
                if let Ok(target) = fs::read_link(entry.path())
                    && inodes
                        .iter()
                        .any(|inode| target.as_os_str() == inode.as_str())
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

pub(super) enum Command {
    Status,
    NewSession,
    EndSession,
    Subscribe,
    GetTree,
    CreateContext,
    CloseContext(String),
    CloseBrowser,
}
impl Command {
    fn encoded(self, id: u64) -> Value {
        let (method, params) = match self {
            Self::Status => ("session.status", json!({})),
            Self::NewSession => ("session.new", json!({"capabilities":{}})),
            Self::EndSession => ("session.end", json!({})),
            Self::Subscribe => (
                "session.subscribe",
                json!({"events":["browsingContext.contextCreated","browsingContext.contextDestroyed"]}),
            ),
            Self::GetTree => ("browsingContext.getTree", json!({})),
            Self::CreateContext => ("browsingContext.create", json!({"type":"tab"})),
            Self::CloseContext(context) => ("browsingContext.close", json!({"context":context})),
            Self::CloseBrowser => ("browser.close", json!({})),
        };
        json!({"id":id,"method":method,"params":params})
    }
}

pub(super) struct Client {
    socket: WebSocketStream<TcpStream>,
    serial: u64,
    usable: bool,
    events: VecDeque<Value>,
}
impl Client {
    pub(super) async fn connect(endpoint: &Endpoint, deadline: Instant) -> Result<Self> {
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(MAX_MESSAGE);
        config.max_frame_size = Some(MAX_MESSAGE);
        let connect = async {
            // A fixed numeric address avoids DNS, inherited proxy settings and
            // endpoint redirects. The HTTP upgrade accepts only its own peer.
            let stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, endpoint.port))
                .await
                .map_err(|_| "connect_failed")?;
            let (socket, _) =
                tokio_tungstenite::client_async_with_config(endpoint.uri(), stream, Some(config))
                    .await
                    .map_err(|_| "handshake_failed")?;
            Ok(Self {
                socket,
                serial: 0,
                usable: true,
                events: VecDeque::new(),
            })
        };
        tokio::time::timeout_at(deadline.into(), connect)
            .await
            .map_err(|_| "handshake_timeout")?
    }

    async fn message(&mut self) -> Result<Value> {
        loop {
            let message = self
                .socket
                .next()
                .await
                .ok_or("disconnected")?
                .map_err(|_| "invalid_message")?;
            match message {
                Message::Text(text) => {
                    return serde_json::from_str(&text).map_err(|_| "invalid_json");
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => return Err("disconnected"),
                _ => return Err("invalid_message"),
            }
        }
    }

    fn event(&mut self, value: Value) -> Result<()> {
        if value["type"] != "event"
            || value["method"].as_str().is_none()
            || !value["params"].is_object()
        {
            return Err("invalid_event");
        }
        if self.events.len() >= MAX_EVENTS {
            return Err("event_overflow");
        }
        self.events.push_back(value);
        Ok(())
    }

    pub(super) async fn request(&mut self, command: Command, deadline: Instant) -> Result<Value> {
        if !self.usable {
            return Err("probe_poisoned");
        }
        // Cancellation after any await cannot authorize reuse of this serial
        // probe. Its owner must close the process; no operation is retried.
        self.usable = false;
        self.serial += 1;
        let id = self.serial;
        let command = command.encoded(id).to_string();
        let result = tokio::time::timeout_at(deadline.into(), async {
            self.socket
                .send(Message::Text(command.into()))
                .await
                .map_err(|_| "send_failed")?;
            loop {
                let value = self.message().await?;
                if value["type"] == "event" {
                    self.event(value)?;
                    continue;
                }
                if value["id"].as_u64() != Some(id) {
                    return Err("unexpected_reply");
                }
                if value["type"] == "success" && value["result"].is_object() {
                    return Ok(value["result"].clone());
                }
                return Err("command_refused");
            }
        })
        .await
        .map_err(|_| "command_timeout")?;
        if result.is_ok() {
            self.usable = true;
        }
        result
    }

    pub(super) async fn wait_event(
        &mut self,
        method: &str,
        context: &str,
        deadline: Instant,
    ) -> Result<()> {
        if !self.usable {
            return Err("probe_poisoned");
        }
        self.usable = false;
        let result = tokio::time::timeout_at(deadline.into(), async {
            loop {
                if let Some(position) = self
                    .events
                    .iter()
                    .position(|e| e["method"] == method && e["params"]["context"] == context)
                {
                    self.events.remove(position);
                    return Ok(());
                }
                let event = self.message().await?;
                self.event(event)?;
            }
        })
        .await
        .map_err(|_| "event_timeout")?;
        if result.is_ok() {
            self.usable = true;
        }
        result
    }
}

#[test]
fn announcement_rejects_external_ambiguous_and_credential_endpoints() {
    assert!(Endpoint::announcement(b"noise\n").unwrap().is_none());
    assert!(
        Endpoint::announcement(b"noise\nWebDriver BiDi listening on ws://127.")
            .unwrap()
            .is_none()
    );
    assert!(
        Endpoint::announcement(b"noise\n\xe4\xbd")
            .unwrap()
            .is_none()
    );
    assert!(
        Endpoint::announcement(b"WebDriver BiDi listening on ws://127.0.0.1:32345\n")
            .unwrap()
            .is_some()
    );
    for raw in [
        "ws://example.test:32345",
        "ws://0.0.0.0:32345",
        "ws://localhost:32345",
        "ws://127.1:32345",
        "ws://127.0.0.1:0",
        "ws://user@127.0.0.1:32345",
        "ws://127.0.0.1:32345/?token=x",
        "wss://127.0.0.1:32345",
        "ws://127.0.0.1:32345/other",
    ] {
        assert!(Endpoint::parse(raw).is_err());
    }
    assert!(Endpoint::announcement(b"WebDriver BiDi listening on ws://127.0.0.1:12345\nWebDriver BiDi listening on ws://127.0.0.1:12346\n").is_err());
}

#[tokio::test]
async fn unowned_endpoint_and_redirect_are_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint {
        port: listener.local_addr().unwrap().port(),
    };
    assert!(!endpoint.owned_by(u32::MAX).unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut input = [0u8; 4096];
        let _ = stream.read(&mut input).await.unwrap();
        stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: ws://192.0.2.1:9999/session\r\nContent-Length: 0\r\n\r\n").await.unwrap();
    });
    assert!(matches!(
        Client::connect(
            &endpoint,
            Instant::now() + std::time::Duration::from_secs(1)
        )
        .await,
        Err("handshake_failed")
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn half_handshake_has_a_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint {
        port: listener.local_addr().unwrap().port(),
    };
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    assert!(matches!(
        Client::connect(
            &endpoint,
            Instant::now() + std::time::Duration::from_millis(50)
        )
        .await,
        Err("handshake_timeout")
    ));
    server.abort();
    let _ = server.await;
}
