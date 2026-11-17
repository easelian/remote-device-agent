//! Explicit Linux acceptance runs, with test-only counters populated by actual
//! protocol replies. The operation, page and process paths are production code.
use super::*;
use axum::{
    Router,
    response::Html,
    routing::{get, post},
};
use std::{fs, io::Write, sync::atomic::AtomicUsize};

struct Fixture {
    browsers: Arc<Browsers>,
    principal: Principal,
    root: PathBuf,
    url: String,
    clicks: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
    log: Mutex<fs::File>,
    release_downloads: Arc<AtomicBool>,
}
impl Fixture {
    async fn new(suffix: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "rdh-d1-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        crate::storage::create_private_dir(&root).unwrap();
        let settings = Settings {
            enabled: true,
            downloads: suffix == "handles",
            executable: Some(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap().into()),
            ..Settings::default()
        };
        let browsers = Arc::new(Browsers::new(
            &settings,
            Some(&root.join("device.json")),
            "d1-acceptance-fixture",
            Shell::default(),
        ));
        let principal = Principal {
            version: 1,
            owner: "a".repeat(64),
            user: "a".repeat(64),
            channel: "ai".into(),
        };
        let clicks = Arc::new(AtomicUsize::new(0));
        let count = clicks.clone();
        let release_downloads = Arc::new(AtomicBool::new(false));
        let held = release_downloads.clone();
        let app=Router::new().route("/",get(||async {Html("<!doctype html><meta charset=utf-8><title>D1 fixture</title><h1>Acceptance fixture</h1><button onclick=\"fetch('/record',{method:'POST'})\">Record once</button>")})).route("/record",post(move||{let count=count.clone();async move{count.fetch_add(1,Ordering::AcqRel);"recorded"}}));
        let app = app.route("/many", get(|| async {Html(format!("<!doctype html><title>Many</title>{}",(0..1001).map(|i|format!("<button>Control {i}</button>")).collect::<String>()))}))
            .route("/noise", get(|| async {Html("<!doctype html><style>body{margin:0}canvas{position:absolute;inset:0}</style><canvas id=c></canvas><h1>Noise ready</h1><script>c.width=innerWidth;c.height=innerHeight;const x=c.getContext('2d'),d=x.createImageData(c.width,c.height);let s=1234567;for(let i=0;i<d.data.length;i++){s=(Math.imul(s,1664525)+1013904223)>>>0;d.data[i]=(i%4===3)?255:s>>>24;}x.putImageData(d,0,0)</script>")}));
        let app = app
            .route(
                "/downloads",
                get(|| async {
                    Html(format!(
                        "<!doctype html><button>Download fixture</button>{}",
                        (0..16)
                            .map(|i| format!("<iframe src='/held?id={i}'></iframe>"))
                            .collect::<String>()
                    ))
                }),
            )
            .route(
                "/held",
                get(move || {
                    let held = held.clone();
                    async move {
                        let first = futures_util::stream::once(async {
                            Ok::<_, std::convert::Infallible>(vec![b'x'; 4096])
                        });
                        let rest = futures_util::stream::once(async move {
                            while !held.load(Ordering::Acquire) {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                            Ok::<_, std::convert::Infallible>(vec![b'x'; 1024 * 1024 - 4096])
                        });
                        let body = axum::body::Body::from_stream(futures_util::StreamExt::chain(
                            first, rest,
                        ));
                        (
                            [
                                ("Content-Type", "application/octet-stream"),
                                ("Content-Disposition", "attachment; filename=fixture.bin"),
                                ("Content-Length", "1048576"),
                            ],
                            body,
                        )
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let destination = PathBuf::from(std::env::var_os("RDH_D1_EVIDENCE").unwrap());
        fs::create_dir_all(&destination).unwrap();
        let label = std::env::var("RDH_D1_LABEL").unwrap();
        let log = Mutex::new(
            fs::File::create(destination.join(format!("{label}-{suffix}.jsonl"))).unwrap(),
        );
        let this = Self {
            browsers,
            principal,
            root,
            url,
            clicks,
            server,
            log,
            release_downloads,
        };
        this.record(json!({"kind":"environment","label":label,"pid":std::process::id(),"affinity":fs::read_to_string("/proc/self/status").unwrap().lines().find(|l|l.starts_with("Cpus_allowed_list:")).unwrap(),"cgroup":fs::read_to_string("/proc/self/cgroup").unwrap(),"limits":cgroup_limits(),"loadavg":fs::read_to_string("/proc/loadavg").unwrap()}));
        this
    }
    fn record(&self, value: Value) {
        let mut log = self.log.lock().unwrap();
        writeln!(log, "{value}").unwrap();
        log.flush().unwrap();
    }
    async fn call(&self, operation: &str, params: Value) -> Result<Value> {
        validate(operation, &params)?;
        self.browsers
            .execute(
                operation,
                &params,
                &self.principal,
                Instant::now() + Duration::from_secs(15),
                None,
            )
            .await
    }
    async fn open(&self) -> String {
        let started = Instant::now();
        let value = self
            .call("browser_open", json!({"url":self.url}))
            .await
            .unwrap();
        self.record(json!({"kind":"open","elapsed_ms":started.elapsed().as_secs_f64()*1000.,"browser":value["browser"]}));
        let id = value["session_id"].as_str().unwrap().to_owned();
        for _ in 0..20 {
            let value = self.observe(&id).await.unwrap();
            if value["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["name"] == "Record once")
            {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("fixture did not finish loading")
    }
    async fn observe(&self, id: &str) -> Result<Value> {
        self.call(
            "browser_observe",
            json!({"session_id":id,"control_epoch":1,"max_nodes":200,"max_chars":20000}),
        )
        .await
    }
    async fn snapshot(&self, id: &str) -> (Value, PathBuf, u32) {
        let slot = self.browsers.sessions.lock().unwrap()[id].clone();
        let session = slot.session.lock().await;
        let process = session.process.as_ref().unwrap();
        let mut metrics = process.engine.test_metrics();
        metrics["references"] = session.references.len().into();
        metrics["cursor"] = session.read.is_some().into();
        metrics["targets"] = session.targets.lock().unwrap().len().into();
        let owner: Value =
            serde_json::from_slice(&fs::read(process.profile.join("owner.json")).unwrap()).unwrap();
        (
            metrics,
            process.profile.clone(),
            owner["pid"].as_u64().unwrap() as u32,
        )
    }
    async fn close_session(&self, id: &str) {
        let (metrics, directory, pid) = self.snapshot(id).await;
        assert_eq!(metrics["pending"], 0);
        assert_eq!(metrics["native_objects"], 0);
        assert_eq!(metrics["streams"], 0);
        let start = Instant::now();
        self.call("browser_control", json!({"session_id":id,"action":"close"}))
            .await
            .unwrap();
        let residual = group(pid);
        self.record(json!({"kind":"close","elapsed_ms":start.elapsed().as_secs_f64()*1000.,"remaining_processes":residual,"directory_remaining":directory.exists(),"remaining_sessions":self.browsers.sessions.lock().unwrap().len(),"agent_rss_kib":rss(std::process::id()),"before_close":metrics}));
        assert!(residual.is_empty());
        assert!(!directory.exists());
        assert!(!self.browsers.sessions.lock().unwrap().contains_key(id));
    }
    async fn finish(&self) {
        assert!(self.browsers.close().await);
        self.browsers.release_stopped_storage();
        let namespace = self.browsers.root.as_ref().unwrap();
        let lock =
            crate::profile_lock::ProfileLocks::acquire(&[crate::browser::storage_lock(namespace)])
                .unwrap();
        drop(lock);
        self.record(json!({"kind":"finished","namespace_lock_reacquired":true,"sessions":self.browsers.sessions.lock().unwrap().len(),"agent_rss_kib":rss(std::process::id()),"loadavg":fs::read_to_string("/proc/loadavg").unwrap()}));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        } else {
            eprintln!("Preserved D1 fixture: {}", self.root.display());
        }
    }
}
fn rss(pid: u32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0)
}
fn group(pgid: u32) -> Vec<u32> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(") ")
                        .and_then(|(_, fields)| fields.split_whitespace().nth(2))
                        .and_then(|s| s.parse::<u32>().ok())
                })
                == Some(pgid)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit D1 release-profile performance acceptance; requires evidence destination and native browser"]
async fn release_performance_samples() {
    let fixture = Fixture::new("performance").await;
    let id = fixture.open().await;
    for operation in ["observe", "click", "screenshot"] {
        for iteration in 0..120 {
            let setup = Instant::now();
            let reference = if operation == "click" {
                let observation = fixture.observe(&id).await.unwrap();
                observation["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["name"] == "Record once")
                    .unwrap()["ref"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            } else {
                String::new()
            };
            let setup_ms = setup.elapsed().as_secs_f64() * 1000.;
            let (before, _, pid) = fixture.snapshot(&id).await;
            let count = fixture.clicks.load(Ordering::Acquire);
            let start = Instant::now();
            let outcome=match operation{
                "observe"=>fixture.observe(&id).await,
                "click"=>fixture.call("browser_act",json!({"session_id":id,"control_epoch":1,"action":{"kind":"click","ref":reference}})).await,
                _=>fixture.call("browser_screenshot",json!({"session_id":id,"control_epoch":1})).await,
            };
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
            let (after, _, _) = fixture.snapshot(&id).await;
            let mut row = json!({"kind":"sample","operation":operation,"phase":if iteration<20{"warmup"}else{"measured"},"iteration":iteration,"elapsed_ms":elapsed_ms,"setup_ms":setup_ms,"commands":after["commands"].as_u64().unwrap()-before["commands"].as_u64().unwrap(),"resources":after,"agent_rss_kib":rss(std::process::id()),"browser_rss_kib":group(pid).into_iter().map(rss).sum::<u64>()});
            if operation == "screenshot"
                && let Ok(image) = &outcome
            {
                row["image"] = json!({"width":image["width"],"height":image["height"],"css_pixel_scale":image["css_pixel_scale"],"encoded_bytes":image["data"].as_str().unwrap().len()});
            }
            if let Err(error) = &outcome {
                row["error_code"] = error.code.into();
                row["execution_state"] = error.state.into();
            }
            fixture.record(row);
            outcome.unwrap();
            assert_eq!(after["pending"], 0);
            assert_eq!(after["native_objects"], 0);
            assert_eq!(after["streams"], 0);
            if operation == "click" {
                let until = Instant::now() + Duration::from_secs(2);
                while fixture.clicks.load(Ordering::Acquire) == count && Instant::now() < until {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                assert_eq!(fixture.clicks.load(Ordering::Acquire), count + 1);
            }
        }
    }
    fixture.close_session(&id).await;
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit D1 lifecycle acceptance; 100 real sessions and 10000 observations"]
async fn normal_sessions_and_observation_replacement_release_owned_resources() {
    let fixture = Fixture::new("resources").await;
    for iteration in 0..10 {
        let id = fixture.open().await;
        fixture.close_session(&id).await;
        fixture.record(json!({"kind":"session_warmup","iteration":iteration}));
    }
    for iteration in 0..100 {
        let id = fixture.open().await;
        fixture.observe(&id).await.unwrap();
        fixture.close_session(&id).await;
        fixture.record(json!({"kind":"session_cycle","iteration":iteration,"agent_rss_kib":rss(std::process::id())}));
    }
    let id = fixture.open().await;
    let mut previous = String::new();
    for iteration in 0..10000 {
        let value = fixture.observe(&id).await.unwrap();
        let reference = value["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "Record once")
            .unwrap()["ref"]
            .as_str()
            .unwrap();
        assert_ne!(reference, previous);
        previous = reference.into();
        let (metrics, _, pid) = fixture.snapshot(&id).await;
        assert_eq!(metrics["pending"], 0);
        assert_eq!(metrics["native_objects"], 0);
        assert_eq!(metrics["streams"], 0);
        assert_eq!(metrics["references"], 1);
        assert_eq!(metrics["cursor"], false);
        if !(10..9990).contains(&iteration) || iteration % 100 == 0 {
            fixture.record(json!({"kind":"observation_cycle","iteration":iteration,"metrics":metrics,"agent_rss_kib":rss(std::process::id()),"browser_rss_kib":group(pid).into_iter().map(rss).sum::<u64>()}));
        }
    }
    fixture.close_session(&id).await;
    fixture.finish().await;
}

fn cgroup_limits() -> Value {
    let membership = fs::read_to_string("/proc/self/cgroup").unwrap();
    let relative = membership
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap();
    let root = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    let mut limits = serde_json::Map::new();
    for name in ["memory.max", "cpu.max"] {
        limits.insert(
            name.into(),
            fs::read_to_string(root.join(name)).unwrap().trim().into(),
        );
    }
    // User scopes need not delegate the cpuset controller. The runner also
    // applies taskset; /proc/self/status above records the effective affinity.
    if let Ok(cpus) = fs::read_to_string(root.join("cpuset.cpus.effective")) {
        limits.insert("cpuset.cpus.effective".into(), cpus.trim().into());
    }
    Value::Object(limits)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit native D1 capacity and image-compression acceptance"]
async fn frozen_session_tab_reference_and_image_budgets() {
    let fixture = Fixture::new("bounds").await;
    let first = fixture.open().await;
    let second = fixture.open().await;
    let overflow = fixture
        .call("browser_open", json!({"url":fixture.url}))
        .await
        .unwrap_err();
    assert_eq!(
        (overflow.code, overflow.state),
        ("browser_session_limit", "not_sent")
    );
    assert_eq!(fixture.browsers.sessions.lock().unwrap().len(), 2);
    fixture.close_session(&second).await;
    for _ in 1..MAX_TABS {
        fixture
            .call(
                "browser_act",
                json!({"session_id":first,"control_epoch":1,"action":{"kind":"new_tab"}}),
            )
            .await
            .unwrap();
    }
    let overflow = fixture
        .call(
            "browser_act",
            json!({"session_id":first,"control_epoch":1,"action":{"kind":"new_tab"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (overflow.code, overflow.state),
        ("browser_tab_limit", "not_sent")
    );
    let tabs = fixture
        .call("browser_tabs", json!({"session_id":first}))
        .await
        .unwrap();
    assert_eq!(tabs["tabs"].as_array().unwrap().len(), 8);
    let many = format!("{}many", fixture.url);
    fixture
        .call(
            "browser_act",
            json!({"session_id":first,"control_epoch":1,"action":{"kind":"navigate","url":many}}),
        )
        .await
        .unwrap();
    fixture
        .call(
            "browser_wait",
            json!({"session_id":first,"control_epoch":1,"text_contains":"Control 0"}),
        )
        .await
        .unwrap();
    let observation = fixture
        .call(
            "browser_observe",
            json!({"session_id":first,"control_epoch":1,"max_nodes":1000,"max_chars":64000}),
        )
        .await
        .unwrap();
    let (metrics, _, _) = fixture.snapshot(&first).await;
    assert!(observation["nodes"].as_array().unwrap().len() <= 1000);
    assert!(metrics["references"].as_u64().unwrap() <= 1000);
    assert!(observation["next_cursor"].is_string());
    let overflow = fixture
        .call(
            "browser_observe",
            json!({"session_id":first,"control_epoch":1,"max_nodes":1001,"max_chars":64000}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (overflow.code, overflow.state),
        ("browser_invalid_params", "not_sent")
    );
    fixture.record(json!({"kind":"capacity","sessions":2,"tabs":8,"observation_nodes":observation["nodes"].as_array().unwrap().len(),"references":metrics["references"],"overflows_refused":3}));
    for (label, url) in [
        ("low_entropy", fixture.url.clone()),
        ("high_entropy", format!("{}noise", fixture.url)),
    ] {
        fixture.call("browser_act",json!({"session_id":first,"control_epoch":1,"action":{"kind":"navigate","url":url}})).await.unwrap();
        fixture.call("browser_wait",json!({"session_id":first,"control_epoch":1,"text_contains":if label=="low_entropy"{"Record once"}else{"Noise ready"}})).await.unwrap();
        match fixture
            .call(
                "browser_screenshot",
                json!({"session_id":first,"control_epoch":1}),
            )
            .await
        {
            Ok(image) => {
                let bytes = image["data"].as_str().unwrap().len();
                assert!(bytes <= crate::browser::image::MAX_ENCODED);
                fixture.record(json!({"kind":"image","fixture":label,"encoded_bytes":bytes,"status":"bounded_success"}));
            }
            Err(error) => {
                assert_eq!(error.code, "browser_image_too_large");
                assert_eq!(label, "high_entropy");
                fixture.record(json!({"kind":"image","fixture":label,"status":"correctly_refused","error_code":error.code}));
            }
        }
    }
    fixture.close_session(&first).await;
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit native lease inventory with held authenticated response streams"]
async fn native_handle_inventory_during_concurrent_downloads() {
    let fixture = Fixture::new("handles").await;
    let id = fixture.open().await;
    for _ in 1..8 {
        fixture
            .call(
                "browser_act",
                json!({"session_id":id,"control_epoch":1,"action":{"kind":"new_tab"}}),
            )
            .await
            .unwrap();
    }
    let slot = fixture.browsers.sessions.lock().unwrap()[&id].clone();
    let (engine, targets) = {
        let session = slot.session.lock().await;
        (
            session.process.as_ref().unwrap().engine.clone(),
            session
                .targets
                .lock()
                .unwrap()
                .values()
                .map(|t| t.session.clone())
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(targets.len(), 8);
    let mut navigations = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        let engine = engine.clone();
        let url = format!("{}held?id={index}", fixture.url);
        navigations.push(tokio::spawn(async move {
            engine
                .navigate(&target, &url, Instant::now() + Duration::from_secs(40))
                .await
        }));
    }
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let (metrics, _, _) = fixture.snapshot(&id).await;
        if metrics["streams"].as_u64().unwrap() >= 6 {
            fixture.record(json!({"kind":"held-download-peak","metrics":metrics}));
            break;
        }
        assert!(
            Instant::now() < until,
            "stream inventory timed out: {metrics}; files: {}",
            fixture.call("browser_files", json!({})).await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fixture.release_downloads.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let files = fixture.call("browser_files", json!({})).await.unwrap();
        if files["files"].as_array().unwrap().len() == 8
            && files["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["state"] == "ready")
            && fixture.snapshot(&id).await.0["streams"] == 0
        {
            break;
        }
        assert!(Instant::now() < until, "download inventory did not finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for navigation in navigations {
        let _ = navigation.await.unwrap().unwrap();
    }
    fixture.close_session(&id).await;
    fixture.finish().await;
}
