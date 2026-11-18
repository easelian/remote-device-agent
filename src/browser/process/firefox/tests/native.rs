use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the native Firefox fixture and namespace helper"]
async fn session_survives_the_exit_of_its_launch_thread() {
    let fixture = Fixture::new("native-launch-thread");
    let runtime = tokio::runtime::Handle::current();
    // Browser RPCs can start on a blocking-pool thread that retires while
    // the user is reading the page. The Agent process still owns the session.
    let (mut owner, mut socket) = std::thread::scope(|threads| {
        threads
            .spawn(|| {
                runtime.block_on(async {
                    let mut owner = fixture.spawn(None);
                    let socket = handshake(owner.ready().await.unwrap(), false).await;
                    (owner, socket)
                })
            })
            .join()
            .unwrap()
    });
    let result = request(&mut socket, 2, "browsingContext.getTree", json!({})).await;
    assert!(!result["result"]["contexts"].as_array().unwrap().is_empty());
    request(&mut socket, 3, "browser.close", json!({})).await;
    owner.close(true).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned Firefox, development Agent and private evidence directory"]
async fn lifecycle_and_tcp_egress() {
    let mut fixture = Fixture::new("native-lifecycle-egress");
    let count: usize = std::env::var("RDH_D2B_CYCLES")
        .unwrap_or_else(|_| "100".into())
        .parse()
        .unwrap();
    let http = HttpFixture::start(false).await;
    let https = HttpFixture::start(true).await;
    for iteration in 0..count {
        let started = Instant::now();
        let mut owner = fixture.spawn(None);
        let ready = owner.ready().await.unwrap();
        let record = owner_record(&owner);
        let ns = namespace(&record).unwrap();
        assert_ne!(
            fs::read_link("/proc/self/ns/net").unwrap(),
            fs::read_link(format!("/proc/{}/ns/net", record["init_pid"])).unwrap()
        );
        assert!(namespace_members(&ns).len() >= 2);
        let mut ws = handshake(ready, iteration == 1).await;
        let startup_ms = started.elapsed().as_secs_f64() * 1000.;
        if iteration < 2 {
            let tree = request(&mut ws, 2, "browsingContext.getTree", json!({})).await;
            let context = tree["result"]["contexts"][0]["context"].as_str().unwrap();
            let result = request(
                &mut ws,
                3,
                "browsingContext.navigate",
                json!({"context":context,"url":http.url(),"wait":"complete"}),
            )
            .await;
            assert_eq!(result["type"], "success");
            let result = request(
                &mut ws,
                4,
                "browsingContext.navigate",
                json!({"context":context,"url":https.url(),"wait":"complete"}),
            )
            .await;
            assert_eq!(
                result["type"],
                if iteration == 1 { "success" } else { "error" }
            );
            fixture.record(json!({"kind":"website_egress","iteration":iteration,"http_success":true,"https_success":iteration==1,"self_signed_rejected_by_default":iteration==0,"synthetic_insecure_cert_capability":iteration==1}));
        }
        let result = request(&mut ws, 5, "browser.close", json!({})).await;
        assert_eq!(result["type"], "success");
        let close_started = Instant::now();
        owner.close(true).await.unwrap();
        assert!(close_started.elapsed() < Duration::from_secs(2));
        drop(ws);
        assert!(namespace_members(&ns).is_empty());
        assert!(!owner.directory.exists());
        fixture.record(json!({"kind":"cycle","iteration":iteration,"startup_ms":startup_ms,"close_ms":close_started.elapsed().as_secs_f64()*1000.,"processes_remaining":0,"directory_remaining":false}));
    }
    assert!(http.requests.load(Ordering::SeqCst) >= 2);
    assert!(https.requests.load(Ordering::SeqCst) >= 1);
    fixture.finish();
}

struct HttpFixture {
    port: u16,
    tls: bool,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl HttpFixture {
    async fn start(tls: bool) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let acceptor = if tls {
            use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(
                        include_bytes!("../fixtures/localhost-cert.der").to_vec(),
                    )],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                        include_bytes!("../fixtures/localhost-key.der").to_vec(),
                    )),
                )
                .unwrap();
            Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
        } else {
            None
        };
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = tasks.join_next(), if !tasks.is_empty() => {},
                    accepted = listener.accept(), if tasks.len() < 16 => {
                        let Ok((stream,_)) = accepted else { break; };
                        let tls = acceptor.clone(); let count = count.clone();
                        tasks.spawn(async move {
                            if let Some(tls) = tls { if let Ok(stream) = tls.accept(stream).await { respond(stream, count).await; } }
                            else { respond(stream, count).await; }
                        });
                    }
                }
            }
        });
        Self {
            port,
            tls,
            requests,
            task,
        }
    }
    fn url(&self) -> String {
        format!(
            "{}://localhost:{}/d2b",
            if self.tls { "https" } else { "http" },
            self.port
        )
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn respond<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: T,
    count: Arc<AtomicUsize>,
) {
    let mut bytes = [0; 4096];
    if matches!(tokio::time::timeout(Duration::from_secs(3), stream.read(&mut bytes)).await, Ok(Ok(n)) if n > 0)
    {
        count.fetch_add(1, Ordering::SeqCst);
        let body = "<!doctype html><title>D2b synthetic</title><p id='proof'>native TCP egress</p>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}

#[tokio::test]
async fn owner_crash_worker() {
    let Some(root) = std::env::var_os("RDH_D2B_CRASH_ROOT").map(PathBuf::from) else {
        return;
    };
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let executable = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let stage = std::env::var("RDH_D2B_CRASH_STAGE").unwrap();
    let gate = (stage != "after_handshake").then_some(stage.as_str());
    let mut owner = OwnedFirefox::spawn(&helper, &executable, &root, gate).unwrap();
    let mut ws = None;
    if let Some(stage) = gate {
        wait_stage(&mut owner, stage).await;
    } else {
        ws = Some(handshake(owner.ready().await.unwrap(), false).await);
    }
    let record = owner_record(&owner);
    println!(
        "D2B_READY={}",
        json!({"owner":record,"namespace":namespace(&record),"directory":owner.directory.file_name().unwrap().to_str().unwrap()})
    );
    std::io::stdout().flush().unwrap();
    std::future::pending::<()>().await;
    drop((owner, ws));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "kills only fixture owners/guardians at explicit barriers"]
async fn owner_and_guardian_sigkill_remove_the_entire_namespace() {
    let mut fixture = Fixture::new("native-sigkill");
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    let repeats: usize = std::env::var("RDH_D2B_CRASH_REPEATS")
        .unwrap_or_else(|_| "20".into())
        .parse()
        .unwrap();
    let mut sentinel = fixture.spawn(None);
    let mut sentinel_ws = handshake(sentinel.ready().await.unwrap(), false).await;
    for target in ["owner", "anchor", "init"] {
        for stage in [
            "before_namespace",
            "before_browser",
            "before_endpoint",
            "before_handshake",
            "after_handshake",
        ] {
            if target == "init" && stage == "before_namespace" {
                continue;
            }
            let count = if target == "owner" { repeats } else { 3 };
            for iteration in 0..count {
                let root = fixture
                    .base
                    .join(format!("crash-{target}-{stage}-{iteration}"));
                storage::create_private_dir(&root).unwrap();
                let mut worker = tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "browser::process::firefox::tests::native::owner_crash_worker",
                        "--nocapture",
                    ])
                    .env("RDH_D2B_CRASH_ROOT", &root)
                    .env("RDH_D2B_CRASH_STAGE", stage)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let mut lines = BufReader::new(worker.stdout.take().unwrap()).lines();
                let message: Value = tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        let line = lines
                            .next_line()
                            .await
                            .unwrap()
                            .expect("worker reaches barrier");
                        if let Some(json) = line.strip_prefix("D2B_READY=") {
                            break serde_json::from_str(json).unwrap();
                        }
                    }
                })
                .await
                .unwrap();
                let record = &message["owner"];
                let ns = message["namespace"].as_str().map(PathBuf::from);
                // Retain the namespace object, not just its reusable inode
                // name. Concurrent sessions may create a new namespace after
                // the old init is reaped but before the final /proc scan.
                let namespace_pin = record["init_pid"]
                    .as_u64()
                    .map(|pid| fs::File::open(format!("/proc/{pid}/ns/pid")).unwrap());
                let started = Instant::now();
                if target == "owner" {
                    worker.kill().await.unwrap();
                } else {
                    let key = if target == "anchor" {
                        "pid"
                    } else {
                        "init_pid"
                    };
                    let pid = record[key].as_u64().unwrap() as u32;
                    let birth_key = if target == "anchor" {
                        "start"
                    } else {
                        "init_start"
                    };
                    assert!(
                        crate::process_identity::running(pid, record[birth_key].as_str().unwrap())
                            .unwrap()
                    );
                    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
                }
                wait_retired(record).await;
                let zombies = ns.as_ref().map_or(Vec::new(), |ns| namespace_members(ns));
                for pid in &zombies {
                    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
                    assert!(
                        matches!(
                            stat.rsplit_once(") ").unwrap().1.split_whitespace().next(),
                            Some("Z" | "X")
                        ),
                        "namespace member still executing: {stat}"
                    );
                }
                if target != "owner" {
                    worker.kill().await.unwrap();
                }
                let _ = worker.wait().await;
                let anchor = record["pid"].as_u64().unwrap() as u32;
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe {
                        libc::waitid(
                            libc::P_PID,
                            anchor,
                            &mut info,
                            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                        )
                    },
                    0
                );
                assert_eq!(unsafe { info.si_pid() }, anchor as i32);
                // Reap adopted init before releasing the pinned anchor PID.
                assert!(super::super::super::posix::linux::stopped(anchor).unwrap());
                assert_eq!(
                    unsafe { libc::waitpid(anchor as i32, std::ptr::null_mut(), libc::WNOHANG) },
                    anchor as i32
                );
                if let Some(ns) = ns {
                    let remaining = namespace_members(&ns);
                    assert!(
                        remaining.is_empty(),
                        "namespace {ns:?} still has members {remaining:?}"
                    );
                }
                super::super::super::recover(&root).unwrap();
                drop(namespace_pin);
                assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
                fs::remove_dir(root).unwrap();
                assert_eq!(
                    request(&mut sentinel_ws, 20, "session.status", json!({})).await["type"],
                    "success"
                );
                fixture.record(json!({"kind":"sigkill","target":target,"stage":stage,"iteration":iteration,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"reaped_namespace_witnesses":zombies.len(),"processes_remaining":0,"recovered":true,"other_session_alive":true}));
            }
        }
    }
    request(&mut sentinel_ws, 21, "browser.close", json!({})).await;
    sentinel.close(true).await.unwrap();
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned Firefox and tests cancellation/failure/forced close"]
async fn startup_cancellation_and_stopped_guardian_recover_safely() {
    let mut fixture = Fixture::new("native-cancel-failure");
    for executable in [Path::new("/rdh-d2b-missing"), Path::new("/bin/true")] {
        let mut owner =
            OwnedFirefox::spawn(&fixture.helper, executable, &fixture.root, None).unwrap();
        assert!(owner.ready().await.is_err());
        owner.close(false).await.unwrap();
        fixture.record(
            json!({"kind":"invalid_executable","missing":!executable.exists(),"cleaned":true}),
        );
    }
    for stage in [
        "before_namespace",
        "before_browser",
        "before_endpoint",
        "before_handshake",
        "after_handshake",
    ] {
        for iteration in 0..3 {
            let gate = (stage != "after_handshake").then_some(stage);
            let mut owner = fixture.spawn(gate);
            let mut ws = None;
            if let Some(stage) = gate {
                wait_stage(&mut owner, stage).await;
            } else {
                ws = Some(handshake(owner.ready().await.unwrap(), false).await);
            }
            let record = owner_record(&owner);
            let ns = namespace(&record);
            let directory = owner.directory.clone();
            drop((owner, ws));
            wait_retired(&record).await;
            if let Some(ns) = ns {
                assert!(namespace_members(&ns).is_empty());
            }
            assert!(directory.exists());
            super::super::super::recover(&fixture.root).unwrap();
            fixture.record(json!({"kind":"cancelled_owner","stage":stage,"iteration":iteration,"processes_remaining":0,"recovered":true}));
        }
    }
    let mut owner = fixture.spawn(None);
    let ws = handshake(owner.ready().await.unwrap(), false).await;
    let record = owner_record(&owner);
    let ns = namespace(&record).unwrap();
    assert_eq!(
        unsafe { libc::kill(record["init_pid"].as_i64().unwrap() as i32, libc::SIGSTOP) },
        0
    );
    let started = Instant::now();
    owner.close(true).await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(namespace_members(&ns).is_empty());
    drop(ws);
    fixture.record(json!({"kind":"stopped_init_forced_close","elapsed_ms":started.elapsed().as_secs_f64()*1000.,"processes_remaining":0}));
    fixture.finish();
}
