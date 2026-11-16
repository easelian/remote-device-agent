use super::*;
use crate::{profile_lock::ProfileLocks, storage};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

mod isolation;
mod native;
type Ws = WebSocketStream<tokio::net::TcpStream>;

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    helper: PathBuf,
    executable: PathBuf,
    evidence: fs::File,
    lock: Option<ProfileLocks>,
}
impl Fixture {
    fn new(label: &str) -> Self {
        let destination = PathBuf::from(
            std::env::var_os("RDH_D2B_EVIDENCE").expect("explicit evidence directory required"),
        );
        let base = destination.join(format!(
            "fixture-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        storage::create_private_dir(&base).unwrap();
        let root = base.join(".rdh-browser");
        storage::create_private_dir(&root).unwrap();
        let lock = Some(ProfileLocks::acquire(&[crate::browser::storage_lock(&root)]).unwrap());
        Self {
            base,
            root,
            helper: PathBuf::from(
                std::env::var_os("RDH_BIDI_HELPER").expect("RDH_BIDI_HELPER required"),
            ),
            executable: PathBuf::from(
                std::env::var_os("RDH_FIREFOX_EXECUTABLE")
                    .expect("RDH_FIREFOX_EXECUTABLE required"),
            ),
            evidence: fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(destination.join(format!("{label}.jsonl")))
                .unwrap(),
            lock,
        }
    }
    fn spawn(&self, gate: Option<&str>) -> OwnedFirefox {
        OwnedFirefox::spawn(&self.helper, &self.executable, &self.root, gate).unwrap()
    }
    fn record(&mut self, value: Value) {
        writeln!(self.evidence, "{value}").unwrap();
        self.evidence.flush().unwrap();
    }
    fn finish(mut self) {
        super::super::recover(&self.root).unwrap();
        assert_eq!(fs::read_dir(&self.root).unwrap().count(), 0);
        self.lock.take().unwrap().release().unwrap();
        let mut lock = ProfileLocks::acquire(&[crate::browser::storage_lock(&self.root)]).unwrap();
        lock.release().unwrap();
        self.record(json!({"kind":"finished","directories_remaining":0,"lock_reacquired":true}));
        fs::remove_dir_all(&self.base).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("D2b fixture retained: {}", self.base.display());
        }
    }
}

async fn request(ws: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    ws.send(Message::Text(
        json!({"id":id,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = ws.next().await.expect("connection remains open").unwrap();
            if let Message::Text(text) = message {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        }
    })
    .await
    .unwrap()
}
async fn handshake(ready: Ready, insecure_fixture: bool) -> Ws {
    let (mut ws, _) = tokio_tungstenite::client_async(&ready.url, ready.stream)
        .await
        .unwrap();
    let result = request(
        &mut ws,
        1,
        "session.new",
        json!({"capabilities":{"alwaysMatch":{"acceptInsecureCerts":insecure_fixture}}}),
    )
    .await;
    assert_eq!(result["result"]["capabilities"]["browserName"], "firefox");
    assert_eq!(
        result["result"]["capabilities"]["browserVersion"],
        std::env::var("RDH_FIREFOX_VERSION").unwrap()
    );
    ws
}
fn owner_record(owner: &OwnedFirefox) -> Value {
    serde_json::from_slice(&storage::read_private(&owner.directory.join("owner.json")).unwrap())
        .unwrap()
}
fn namespace(owner: &Value) -> Option<PathBuf> {
    owner["init_pid"]
        .as_u64()
        .and_then(|pid| fs::read_link(format!("/proc/{pid}/ns/pid")).ok())
}
fn namespace_members(namespace: &Path) -> Vec<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            (fs::read_link(entry.path().join("ns/pid")).ok().as_deref() == Some(namespace))
                .then_some(pid)
        })
        .collect()
}
async fn wait_retired(record: &Value) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !retired(record).unwrap() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("namespace must stop within the native close budget");
}
async fn wait_stage(owner: &mut OwnedFirefox, expected: &str) {
    tokio::time::timeout(STARTUP, async {
        loop {
            let (value, mut fds) = ipc::read(&owner.channel).await.unwrap();
            if value["kind"] == "egress" {
                assert_eq!(fds.len(), 1);
                let listener = TcpListener::from(fds.remove(0));
                listener.set_nonblocking(true).unwrap();
                owner.egress = Some(socks::serve(
                    tokio::net::TcpListener::from_std(listener).unwrap(),
                ));
            } else {
                assert_eq!(value, json!({"kind":"stage","stage":expected}));
                assert!(fds.is_empty());
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[test]
fn endpoint_parser_never_resolves_announced_hosts_or_accepts_redirects() {
    assert_eq!(
        endpoint(b"WebDriver BiDi listening on ws://localhost:12345\n").unwrap(),
        Some(12345)
    );
    assert_eq!(
        endpoint(b"WebDriver BiDi listening on ws://127.0.0.1:12345").unwrap(),
        None
    );
    for url in [
        "ws://localhost.evil:1",
        "wss://localhost:1",
        "ws://user@localhost:1",
        "ws://localhost:1/session",
        "ws://localhost:1/?x=y",
        "ws://localhost:0",
    ] {
        assert!(endpoint(format!("WebDriver BiDi listening on {url}\n").as_bytes()).is_err());
    }
    assert!(endpoint(b"WebDriver BiDi listening on ws://localhost:1\nWebDriver BiDi listening on ws://localhost:2\n").is_err());
}
