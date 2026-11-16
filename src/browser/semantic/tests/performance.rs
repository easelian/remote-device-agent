//! Explicit Firefox absolute gate; never part of ordinary unit-test timing.
use super::*;
use observation::{read, reference};

fn budget() -> Value {
    let executable = std::env::current_exe().unwrap();
    assert_eq!(
        executable
            .parent()
            .and_then(|parent| parent.parent())
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str()),
        Some("release"),
        "performance gate requires the release test profile"
    );
    let membership = fs::read_to_string("/proc/self/cgroup").unwrap();
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap();
    let directory = PathBuf::from("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    let cpu = fs::read_to_string(directory.join("cpu.max")).unwrap();
    let memory = fs::read_to_string(directory.join("memory.max")).unwrap();
    let swap = fs::read_to_string(directory.join("memory.swap.max")).unwrap();
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let affinity = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .unwrap()
        .trim();
    assert_eq!(cpu.trim(), "200000 100000");
    assert_eq!(memory.trim(), "4294967296");
    assert_eq!(swap.trim(), "0");
    assert_eq!(affinity, "0-1");
    json!({"cpu_max":cpu.trim(),"memory_max":memory.trim(),"swap_max":swap.trim(),"cpu_affinity":affinity,"cgroup":relative})
}
fn p95(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() * 95).div_ceil(100) - 1]
}
async fn fixture() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let mut body="<!doctype html><meta charset=utf-8><style>iframe{width:24px;height:24px}</style><textarea id='benchmark-input' aria-label='Stable input'></textarea><div id='fixture-nodes'>".to_owned();
    for _ in 0..250 {
        body.push_str("<span></span>");
    }
    body.push_str("</div>");
    for _ in 0..19 {
        body.push_str("<iframe src='/child'></iframe>");
    }
    let child = format!(
        "<!doctype html><meta charset=utf-8><button aria-label='Child'>C</button><div id='fixture-nodes'>{}</div>",
        "<span></span>".repeat(250)
    );
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(move || {
                let body = body.clone();
                async move { axum::response::Html(body) }
            }),
        )
        .route(
            "/child",
            axum::routing::get(move || {
                let child = child.clone();
                async move { axum::response::Html(child) }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "release-profile Firefox gate in verified 2 CPU / 4 GiB cgroup; explicit evidence directory"]
async fn bidi_native_performance_budget() {
    let limits = budget();
    let (url, server) = fixture().await;
    let root = root("bidi-performance");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut startup = Vec::new();
    let mut observations = Vec::new();
    let mut inputs = Vec::new();
    let mut restoring = Vec::new();
    for iteration in 0..30 {
        let started = Instant::now();
        let mut session = engine::bidi::page::Session::open(
            &helper,
            &browser,
            &root,
            policy::Policy::new(Default::default()),
        )
        .await
        .unwrap();
        session.navigate(&url, deadline()).await.unwrap();
        assert_eq!(session.port.frames(deadline()).await.unwrap().len(), 20);
        startup.push(started.elapsed().as_secs_f64() * 1000.);
        if iteration == 0 {
            let page = Page::new(session.port.clone());
            assert_eq!(
                session
                    .port
                    .fixture(
                        "document.querySelectorAll('#fixture-nodes>span').length+Array.from(document.querySelectorAll('iframe')).reduce((total,frame)=>total+frame.contentDocument.querySelectorAll('#fixture-nodes>span').length,0)",
                        deadline()
                    )
                    .await
                    .unwrap(),
                5000
            );
            for sample in 0..120 {
                let at = Instant::now();
                let observation = read(&page).await;
                assert!(
                    observation.complete,
                    "performance fixture must cover all twenty documents"
                );
                assert_eq!(
                    observation
                        .rows
                        .iter()
                        .map(|row| row.context)
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    20
                );
                let elapsed = at.elapsed().as_secs_f64() * 1000.;
                assert!(
                    observation
                        .rows
                        .iter()
                        .any(|row| row.facts.id == "benchmark-input")
                );
                if sample >= 20 {
                    observations.push(elapsed);
                }
            }
            for sample in 0..120 {
                let at = Instant::now();
                let target = reference(&read(&page).await, "benchmark-input");
                let restore_ms = at.elapsed().as_secs_f64() * 1000.;
                let at = Instant::now();
                page.act(
                    target,
                    Action::ReplaceText(format!("sample-{sample}")),
                    deadline(),
                )
                .await
                .unwrap();
                let elapsed = at.elapsed().as_secs_f64() * 1000.;
                if sample >= 20 {
                    inputs.push(elapsed);
                    restoring.push(restore_ms);
                }
            }
        }
        session.close().await.unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }
    let result = json!({"profile":"release","limits":limits,"dom_fixture_elements":5000,"frames":20,"warmups":20,"startup_ms":startup,"observation_ms":observations,"input_ms":inputs,"fresh_observation_before_input_ms":restoring,"p95_ms":{"startup":p95(&startup),"observation":p95(&observations),"input":p95(&inputs)},"thresholds_ms":{"startup":10000,"observation":3000,"input":1000},"failures":0});
    record("bidi-performance.json", &result);
    eprintln!("performance gate: {}", result["p95_ms"]);
    server.abort();
    fs::remove_dir(root).unwrap();
    assert!(p95(&startup) <= 10000.);
    assert!(p95(&observations) <= 3000.);
    assert!(p95(&inputs) <= 1000.);
}
