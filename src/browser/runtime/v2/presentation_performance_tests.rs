//! Optional projection CPU cost on the fixed full-capacity native page.
use super::*;

fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(fraction * sorted.len() as f64).ceil() as usize - 1]
}

async fn performance(backend: &'static str) {
    let protocol: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/observation-delta-v1/performance-protocol.json"
    ))
    .unwrap();
    let parameters: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/observation-delta-v1/memory-protocol.json"
    ))
    .unwrap();
    assert_eq!(protocol["batches"], 3);
    assert_eq!(protocol["mode_order"], json!(["off", "on"]));
    assert_eq!(protocol["warmup_observations"], 20);
    assert_eq!(protocol["measured_observations"], 100);
    let destination = PathBuf::from(std::env::var_os("RDH_DELTA_EVIDENCE").unwrap());
    fs::create_dir_all(&destination).unwrap();
    let mut output =
        fs::File::create_new(destination.join(format!("{backend}-performance.jsonl"))).unwrap();
    let (url, server) = capacity_site().await;
    let mut batches = Vec::new();
    let mut closed_batches = Vec::new();
    let mut ordinary_commands = None;
    let mut session_ids = std::collections::HashSet::new();
    for batch in 0..3 {
        for enabled in [false, true] {
            // Independent batches need the same browser age and startup state.
            // Reusing one process makes later modes inherit earlier GC history.
            let mut fixture = Fixture::new(backend).await;
            assert!(session_ids.insert(fixture.id.clone()));
            let session_id = fixture.id.clone();
            let result = std::panic::AssertUnwindSafe(async {
                fixture.navigate(&url).await;
                fixture.fixture(&format!("(()=>{{window.capacity({},{},{});return true}})()",parameters["tail"],parameters["ascii"],parameters["title_ascii"])).await;
                fixture.read(None, false, false).await;
                let mut revision: Option<String> = None;
                let mut samples = Vec::new();
                let mut preparations = Vec::new();
                for iteration in 0..120 {
                    let entry = fixture.read(revision.as_deref(), false, enabled).await;
                    assert_eq!(entry["agent"]["coverage"]["complete"], true);
                    let commands = entry["commands"].as_u64().unwrap();
                    assert_eq!(*ordinary_commands.get_or_insert(commands), commands);
                    if enabled {
                        assert_eq!(entry["agent"]["presentation"]["cacheable"], true);
                        revision = Some(entry["agent"]["presentation"]["revision"].as_str().unwrap().to_owned());
                        if iteration >= 1 { assert_eq!(entry["cache"]["encoded_bytes"], 2 * MAX_BASELINE); }
                    } else { assert_eq!(entry["cache"]["encoded_bytes"], 0); }
                    let elapsed = entry["elapsed_ms"].as_f64().unwrap();
                    let preparation = if enabled { entry["cache"]["preparation_nanos"].as_u64().unwrap() as f64 / 1_000_000. } else { 0. };
                    let row = json!({"batch":batch,"enabled":enabled,"session_id":session_id,"iteration":iteration,"phase":if iteration<20 {"warmup"}else{"measured"},
                        "elapsed_ms":elapsed,"preparation_ms":preparation,"commands":commands,"cache":entry["cache"]});
                    writeln!(output, "{row}").unwrap();
                    output.flush().unwrap();
                    if iteration >= 20 { samples.push(elapsed); preparations.push(preparation); }
                }
                json!({"batch":batch,"enabled":enabled,"session_id":session_id,"p50_ms":percentile(&samples,0.5),"p95_ms":percentile(&samples,0.95),
                    "preparation_p50_ms":percentile(&preparations,0.5),"preparation_p95_ms":percentile(&preparations,0.95),"samples":samples.len()})
            }).catch_unwind().await;
            let closed = fixture.close().await;
            closed_batches.push(
                json!({"batch":batch,"enabled":enabled,"session_id":session_id,"closed":closed}),
            );
            fs::write(
                destination.join(format!("{backend}-performance-close.json")),
                serde_json::to_vec_pretty(&closed_batches).unwrap(),
            )
            .unwrap();
            match result {
                Ok(value) => batches.push(value),
                Err(error) => {
                    server.abort();
                    std::panic::resume_unwind(error);
                }
            }
        }
    }
    server.abort();
    assert_eq!(session_ids.len(), 6);
    let commands = ordinary_commands.unwrap();
    let mut gates = Vec::new();
    let mut stable = true;
    for (metric, relative, absolute) in [("p50_ms", 0.10, 5_f64), ("p95_ms", 0.15, 10_f64)] {
        let baseline: Vec<_> = batches
            .iter()
            .filter(|row| row["enabled"] == false)
            .map(|row| row[metric].as_f64().unwrap())
            .collect();
        let minimum = baseline.iter().copied().reduce(f64::min).unwrap();
        let maximum = baseline.iter().copied().reduce(f64::max).unwrap();
        stable &= maximum - minimum <= (minimum * relative).max(absolute);
        for pair in batches.chunks_exact(2) {
            let off = pair[0][metric].as_f64().unwrap();
            let on = pair[1][metric].as_f64().unwrap();
            let limit = (off * relative).max(absolute);
            gates.push(json!({"batch":pair[0]["batch"],"metric":metric,"off_ms":off,"on_ms":on,"increase_ms":on-off,"limit_ms":limit,"passed":on-off<=limit}));
        }
    }
    let passed = gates.iter().all(|gate| gate["passed"] == true);
    let summary = json!({"backend":backend,"protocol":protocol,"batches":batches,"gates":gates,"commands_per_observation":commands,
        "baseline_stable":stable,"status":if !stable {"conditions_insufficient"}else if passed {"passed"}else{"failed"},"closed":closed_batches});
    fs::write(
        destination.join(format!("{backend}-performance-summary.json")),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    assert!(stable && passed, "{summary}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "frozen cache performance acceptance in an exclusive 2 CPU / 4 GiB release fixture"]
async fn native_delta_performance_cdp() {
    performance("cdp").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "frozen cache performance acceptance in an exclusive 2 CPU / 4 GiB release fixture"]
async fn native_delta_performance_bidi() {
    performance("bidi").await;
}
