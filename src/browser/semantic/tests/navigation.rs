use super::*;
use policy::{Decisions, Metrics, Policy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Counters {
    connections: AtomicUsize,
    requests: AtomicUsize,
    redirects: AtomicUsize,
}
struct Fixture {
    allowed: String,
    denied: String,
    allow: Arc<Counters>,
    deny: Arc<Counters>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn open() -> Self {
        let allowed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let denied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let allowed_url = format!("http://{}", allowed.local_addr().unwrap());
        let denied_url = format!("http://{}", denied.local_addr().unwrap());
        let allow = Arc::new(Counters::default());
        let deny = Arc::new(Counters::default());
        let mut tasks = Vec::new();
        for (listener, counters, redirect) in [
            (allowed, allow.clone(), format!("{denied_url}/destination")),
            (denied, deny.clone(), format!("{allowed_url}/destination")),
        ] {
            tasks.push(tokio::spawn(async move {
                let mut connections=tokio::task::JoinSet::new();loop {tokio::select!{
                    _=connections.join_next(),if !connections.is_empty()=>{},
                    accepted=listener.accept()=>{let (mut socket,_)=accepted.unwrap();counters.connections.fetch_add(1,Ordering::AcqRel);let counters=counters.clone();let redirect=redirect.clone();
                        if connections.len()>=128{continue;}
                        connections.spawn(async move {let mut bytes=Vec::new();let mut block=[0u8;1024];while bytes.len()<8192&&!bytes.windows(4).any(|s|s==b"\r\n\r\n"){
                            let count=match tokio::time::timeout(Duration::from_secs(2),socket.read(&mut block)).await{Ok(Ok(count)) if count>0=>count,_=>return};bytes.extend_from_slice(&block[..count]);
                        }
                        let text=String::from_utf8_lossy(&bytes);let path=text.lines().next().and_then(|line|line.split_whitespace().nth(1)).unwrap_or("");
                        if path=="/favicon.ico"{let _=socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;return;}
                        counters.requests.fetch_add(1,Ordering::AcqRel);
                        let response=if path=="/redirect"{counters.redirects.fetch_add(1,Ordering::AcqRel);format!("HTTP/1.1 302 Found\r\nLocation: {redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")}else{
                            let body="<!doctype html><meta charset=utf-8><title>Policy fixture</title><button id='policy-root'>Allowed page</button>";format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
                        };let _=socket.write_all(response.as_bytes()).await;
                        });
                    }
                }}
            }));
        }
        Self {
            allowed: allowed_url,
            denied: denied_url,
            allow,
            deny,
            tasks,
        }
    }
    fn policy(&self) -> Policy {
        let url = url::Url::parse(&self.denied).unwrap();
        Policy::new(
            serde_json::from_value(
                json!({"public":"deny","deny":[format!("127.0.0.1:{}",url.port().unwrap())]}),
            )
            .unwrap(),
        )
    }
    fn snapshot(&self) -> Value {
        json!({"allowed_connections":self.allow.connections.load(Ordering::Acquire),"allowed_requests":self.allow.requests.load(Ordering::Acquire),"allowed_redirect_requests":self.allow.redirects.load(Ordering::Acquire),"denied_connections":self.deny.connections.load(Ordering::Acquire),"denied_requests":self.deny.requests.load(Ordering::Acquire)})
    }
}
#[derive(Clone, Copy, Debug)]
enum Trigger {
    Script,
    Frame,
    Popup,
}
trait Harness: Send + Sync {
    fn visit(&self, url: &str) -> impl Future<Output = Result<()>> + Send;
    fn trigger(&self, kind: Trigger, url: &str) -> impl Future<Output = Result<()>> + Send;
    fn metrics(&self) -> Metrics;
}
fn expression(kind: Trigger, url: &str) -> String {
    let url = serde_json::to_string(url).unwrap();
    match kind {
        Trigger::Script => format!("(setTimeout(()=>location.assign({url}),0),true)"),
        Trigger::Frame => format!(
            "(()=>{{document.getElementById('policy-frame')?.remove();let e=document.createElement('iframe');e.id='policy-frame';e.src={url};document.body.append(e);return true;}})()"
        ),
        Trigger::Popup => format!(
            "(()=>{{let e=document.createElement('button');e.id='policy-popup';e.textContent='Open policy popup';e.onclick=()=>{{globalThis.lastPopup=window.open({url});}};document.body.append(e);return true;}})()"
        ),
    }
}
macro_rules! native_harness {
    ($ty:ty) => {
        impl Harness for $ty {
            async fn visit(&self, url: &str) -> Result<()> {
                self.navigate(url, deadline()).await
            }
            async fn trigger(&self, kind: Trigger, url: &str) -> Result<()> {
                self.port
                    .fixture(&expression(kind, url), deadline())
                    .await?;
                if matches!(kind, Trigger::Popup) {
                    let page = Page::new(self.port.clone());
                    let read = page
                        .observe(
                            Query {
                                find: Some("Open policy popup".into()),
                                ..Default::default()
                            },
                            200,
                            64000,
                            None,
                            deadline(),
                        )
                        .await?;
                    let reference = observation::reference(&read, "policy-popup");
                    page.act(reference, Action::Click, deadline()).await?;
                    assert_eq!(
                        self.port
                            .fixture("Boolean(globalThis.lastPopup)", deadline())
                            .await?,
                        true,
                        "popup must actually be created"
                    );
                }
                Ok(())
            }
            fn metrics(&self) -> Metrics {
                self.policy_metrics()
            }
        }
    };
}
native_harness!(engine::semantic_cdp::Session);
#[cfg(target_os = "linux")]
native_harness!(engine::bidi::page::Session);

struct FakeBrowser {
    policy: Policy,
    decisions: Decisions,
    next: AtomicUsize,
}
impl FakeBrowser {
    async fn navigation(&self, url: &str) -> Result<()> {
        let request = self.next.fetch_add(1, Ordering::Relaxed).to_string();
        let mut next = url.to_owned();
        for hop in 0..4 {
            let token = self
                .decisions
                .claim("fake-context", &request, hop, &next, true, true)?;
            let allow = token.allows(&self.policy).await;
            token.settled(allow);
            if !allow {
                return Ok(());
            }
            let url = url::Url::parse(&next).unwrap();
            let mut socket =
                tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap()))
                    .await
                    .unwrap();
            socket
                .write_all(
                    format!(
                        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                        url.path(),
                        url.host_str().unwrap()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).await.unwrap();
            if let Some(location) = reply
                .lines()
                .find_map(|line| line.strip_prefix("Location: "))
            {
                next = location.into();
            } else {
                return Ok(());
            }
        }
        Err(crate::browser::Failure::new("fixture_redirect_limit"))
    }
}
impl Harness for FakeBrowser {
    async fn visit(&self, url: &str) -> Result<()> {
        if !self.policy.allows(url, deadline()).await {
            return Err(crate::browser::Failure::new("browser_site_denied"));
        }
        self.navigation(url).await
    }
    async fn trigger(&self, kind: Trigger, url: &str) -> Result<()> {
        if matches!(kind, Trigger::Popup | Trigger::Frame) {
            self.decisions.context_created();
        }
        self.navigation(url).await
    }
    fn metrics(&self) -> Metrics {
        self.decisions.metrics()
    }
}
async fn wait<H: Harness>(harness: &H, condition: impl Fn(&Metrics) -> bool) {
    let until = deadline();
    loop {
        if condition(&harness.metrics()) {
            break;
        }
        assert!(
            Instant::now() < until,
            "navigation metrics did not settle: {}",
            serde_json::to_string(&harness.metrics()).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
async fn assertions<H: Harness>(harness: &H, fixture: &Fixture) -> Value {
    let root = format!("{}/root", fixture.allowed);
    let deny = format!("{}/destination", fixture.denied);
    let redirect = format!("{}/redirect", fixture.allowed);
    harness.visit(&root).await.unwrap();
    let before = fixture.snapshot();
    let error = harness.visit(&deny).await.unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_site_denied", "not_sent")
    );
    assert_eq!(fixture.snapshot()["denied_connections"], 0);
    let mut cases = vec![json!({"trigger":"direct","before":before,"after":fixture.snapshot()})];
    for (kind, url) in [
        (Trigger::Script, deny.as_str()),
        (Trigger::Frame, deny.as_str()),
        (Trigger::Popup, deny.as_str()),
        (Trigger::Script, redirect.as_str()),
    ] {
        harness.visit(&root).await.unwrap();
        let before = harness.metrics();
        let counts = fixture.snapshot();
        harness.trigger(kind, url).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        eprintln!(
            "policy triggered {:?}: {} / {}",
            kind,
            fixture.snapshot(),
            serde_json::to_string(&harness.metrics()).unwrap()
        );
        wait(harness, |metrics| {
            metrics.denied > before.denied && metrics.active == 0
        })
        .await;
        eprintln!(
            "policy evidence: {}",
            json!({"trigger":format!("{kind:?}"),"redirect":url==redirect,"before":counts,"after":fixture.snapshot(),"decisions":harness.metrics()})
        );
        // BC-30 freezes first-document-request interception. BC-31 explicitly
        // distinguishes that from preventing a TCP preconnection. Preserve
        // both measured counters without inventing the stronger guarantee.
        assert_eq!(fixture.snapshot()["denied_requests"], 0);
        if matches!(kind, Trigger::Popup) {
            wait(harness, |metrics| metrics.contexts > before.contexts).await;
        }
        if url == redirect {
            assert_eq!(
                fixture.snapshot()["allowed_redirect_requests"]
                    .as_u64()
                    .unwrap(),
                counts["allowed_redirect_requests"].as_u64().unwrap() + 1
            );
            assert!(harness.metrics().redirects > before.redirects);
        }
        cases.push(json!({"trigger":format!("{kind:?}"),"redirect":url==redirect,"before":counts,"after":fixture.snapshot(),"decisions":harness.metrics()}));
    }
    for kind in [Trigger::Frame, Trigger::Popup] {
        harness.visit(&root).await.unwrap();
        let before = fixture.allow.requests.load(Ordering::Acquire);
        harness
            .trigger(kind, &format!("{}/allowed-child", fixture.allowed))
            .await
            .unwrap();
        let until = deadline();
        while fixture.allow.requests.load(Ordering::Acquire) == before {
            assert!(Instant::now() < until);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    assert_eq!(fixture.snapshot()["denied_requests"], 0);
    json!({"cases":cases,"positive_frame_popup_requests":true,"connection_prevention_claimed":false,"final":fixture.snapshot(),"decisions":harness.metrics()})
}
#[tokio::test]
async fn fake_navigation_uses_the_first_request_and_redirect_assertions() {
    let fixture = Fixture::open().await;
    let fake = FakeBrowser {
        policy: fixture.policy(),
        decisions: Decisions::default(),
        next: AtomicUsize::new(1),
    };
    assertions(&fake, &fixture).await;
}
#[tokio::test]
async fn duplicate_hops_capacity_and_unattributed_requests_are_fenced() {
    let ledger = Decisions::default();
    let rules = Policy::new(Default::default());
    let first = ledger
        .claim("a", "native", 0, "http://example.com/", true, true)
        .unwrap();
    assert!(first.allows(&rules).await);
    first.settled(true);
    assert!(
        ledger
            .claim("a", "native", 0, "http://example.com/", true, true)
            .is_err()
    );
    ledger
        .claim("a", "native", 1, "http://example.com/", true, true)
        .unwrap()
        .settled(false);
    let unknown = ledger
        .claim("", "unknown", 0, "http://example.com/", true, false)
        .unwrap();
    assert!(!unknown.allows(&rules).await);
    unknown.settled(false);
    let tokens = (0..32)
        .map(|n| {
            ledger
                .claim(
                    "a",
                    &format!("new-{n}"),
                    0,
                    "http://example.com/",
                    true,
                    true,
                )
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        ledger
            .claim("a", "overflow", 0, "http://example.com/", true, true)
            .is_err()
    );
    drop(tokens);
    assert_eq!(ledger.metrics().active, 0);
    let independent = Decisions::default();
    assert!(
        independent
            .claim("a", "native", 0, "http://example.com/", true, true)
            .is_ok()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Chrome and evidence directory"]
async fn cdp_native_navigation_policy() {
    let fixture = Fixture::open().await;
    let root = root("cdp-policy");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut session =
        engine::semantic_cdp::Session::launch(&browser, &root, fixture.policy(), deadline())
            .await
            .unwrap();
    record("cdp-navigation.json", &assertions(&session, &fixture).await);
    session.close().await.unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Firefox helper and evidence directory"]
async fn bidi_native_navigation_policy() {
    let fixture = Fixture::open().await;
    let root = root("bidi-policy");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut session = engine::bidi::page::Session::open(&helper, &browser, &root, fixture.policy())
        .await
        .unwrap();
    record(
        "bidi-navigation.json",
        &assertions(&session, &fixture).await,
    );
    session.close().await.unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
