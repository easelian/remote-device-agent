//! Frozen cache-off/cache-on memory cycles, independent of the original four
//! public Agent RSS groups. Never calibrate parameters during these samples.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(serde::Serialize)]
struct Process {
    pid: u32,
    parent: u32,
    start: String,
    kind: &'static str,
    rss_kib: u64,
    pss_kib: Option<u64>,
    background_renderer: bool,
}

fn kib(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn identity(pid: u32) -> Option<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.to_string()))
}

fn process(pid: u32) -> Option<Process> {
    let (parent, start) = identity(pid)?;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let pss = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|text| kib(&text, "Pss:"));
    let command = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let background_renderer = command
        .split(|byte| *byte == 0 || byte.is_ascii_whitespace())
        .any(|arg| arg == b"--extension-process" || arg == b"--top-chrome-webui");
    let executable = fs::read_link(format!("/proc/{pid}/exe")).ok();
    let helper = std::env::var_os("RDH_BIDI_HELPER").map(PathBuf::from);
    Some(Process {
        pid,
        parent,
        start,
        kind: if pid == std::process::id() {
            "host"
        } else if executable.is_some() && executable == helper {
            "helper"
        } else {
            "browser"
        },
        rss_kib: kib(&status, "VmRSS:").unwrap_or(0),
        pss_kib: pss,
        background_renderer,
    })
}

fn snapshot() -> (Value, Vec<(u32, String)>) {
    let host = process(std::process::id()).unwrap();
    assert!(
        host.pss_kib.is_some(),
        "The frozen Linux fixture requires readable host PSS"
    );
    let candidates: BTreeMap<_, _> = fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| identity(pid).map(|(parent, _)| (pid, parent)))
        .collect();
    let mut owned = BTreeSet::from([host.pid]);
    loop {
        let previous = owned.len();
        for (pid, parent) in &candidates {
            if owned.contains(parent) {
                owned.insert(*pid);
            }
        }
        if previous == owned.len() {
            break;
        }
    }
    owned.remove(&host.pid);
    let children: Vec<_> = owned.into_iter().filter_map(process).collect();
    let identities = children.iter().map(|p| (p.pid, p.start.clone())).collect();
    let value = json!({"host":host,"processes":children,
        "browser_rss_kib":children.iter().filter(|p|p.kind=="browser").map(|p|p.rss_kib).sum::<u64>(),
        "helper_rss_kib":children.iter().filter(|p|p.kind=="helper").map(|p|p.rss_kib).sum::<u64>(),
        "tree_pss_kib":children.iter().filter_map(|p|p.pss_kib).sum::<u64>(),
        "pss_readable_processes":children.iter().filter(|p|p.pss_kib.is_some()).count()});
    (value, identities)
}

async fn fill(a: &mut Fixture, b: &mut Fixture, enabled: bool) -> Value {
    let mut revisions: [Option<String>; 2] = [None, None];
    let mut records = Vec::new();
    for _ in 0..3 {
        let (one, two) = tokio::join!(
            a.read(revisions[0].as_deref(), false, enabled),
            b.read(revisions[1].as_deref(), false, enabled)
        );
        for (index, entry) in [&one, &two].into_iter().enumerate() {
            assert_eq!(entry["agent"]["coverage"]["complete"], true);
            if enabled {
                assert_eq!(entry["agent"]["presentation"]["cacheable"], true);
                assert_eq!(entry["agent"]["presentation"]["node_count"], 8);
                revisions[index] = Some(
                    entry["agent"]["presentation"]["revision"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        records.push(json!({"commands":[one["commands"],two["commands"]],
            "elapsed_ms":[one["elapsed_ms"],two["elapsed_ms"]],"cache":[one["cache"],two["cache"]]}));
    }
    let cache = [
        a.session.presentation.metrics(),
        b.session.presentation.metrics(),
    ];
    let expected = if enabled { 4 * MAX_BASELINE } else { 0 };
    assert_eq!(
        cache
            .iter()
            .map(|value| value["encoded_bytes"].as_u64().unwrap())
            .sum::<u64>(),
        expected as u64
    );
    assert_eq!(
        cache
            .iter()
            .map(|value| value["allocated_bytes"].as_u64().unwrap())
            .sum::<u64>(),
        expected as u64
    );
    assert!(
        cache
            .iter()
            .all(|value| value["construction_peak"].as_u64().unwrap() <= MAX_CONSTRUCTION as u64)
    );
    json!({"observations":records,"cache":cache})
}

fn median(mut values: Vec<u64>) -> u64 {
    assert!(!values.is_empty());
    values.sort_unstable();
    (values[(values.len() - 1) / 2] + values[values.len() / 2]) / 2
}

async fn memory(backend: &'static str, enabled: bool) {
    let protocol: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/observation-delta-v1/memory-protocol.json"
    ))
    .unwrap();
    assert_eq!(protocol["baseline_bytes"], MAX_BASELINE);
    assert_eq!(protocol["retained_bytes_when_full"], 4 * MAX_BASELINE);
    assert_eq!(protocol["warmup_cycles"], 20);
    assert_eq!(protocol["measured_cycles"], 100);
    assert_eq!(protocol["concurrent_sessions"], 2);
    assert_eq!(protocol["observations_per_fill"], 3);
    assert_eq!(protocol["fills_per_cycle"], 2);
    assert_eq!(protocol["invalidations_per_cycle"], 1);
    assert_eq!(protocol["native_nodes"], 8);
    assert_eq!(protocol["max_nodes"], 1000);
    assert_eq!(protocol["max_chars"], 64000);
    let destination = PathBuf::from(std::env::var_os("RDH_DELTA_EVIDENCE").unwrap());
    fs::create_dir_all(&destination).unwrap();
    let label = format!("{backend}-{}", if enabled { "on" } else { "off" });
    let mut output =
        fs::File::create_new(destination.join(format!("{label}-memory.jsonl"))).unwrap();
    let (url, server) = capacity_site().await;
    assert_eq!(url.len() as u64, protocol["url_bytes"].as_u64().unwrap());
    let mut measurements = Vec::new();
    for cycle in 0..120 {
        let mut a = Fixture::new(backend).await;
        let mut b = Fixture::new(backend).await;
        let result = std::panic::AssertUnwindSafe(async {
            tokio::join!(a.navigate(&url), b.navigate(&url));
            let setup = format!(
                "(()=>{{window.capacity({},{},{});return true}})()",
                protocol["tail"], protocol["ascii"], protocol["title_ascii"]
            );
            tokio::join!(a.fixture(&setup), b.fixture(&setup));
            let first = fill(&mut a, &mut b, enabled).await;
            // The ordinary observation path clears retained display content.
            // Reuse the same live sessions to prove repeated invalidation.
            let (one, two) = tokio::join!(a.read(None, false, false), b.read(None, false, false));
            assert_eq!(one["cache"]["encoded_bytes"], 0);
            assert_eq!(two["cache"]["encoded_bytes"], 0);
            drop((one, two));
            let second = fill(&mut a, &mut b, enabled).await;
            let (opened, identities) = snapshot();
            if backend == "cdp" {
                assert!(
                    opened["processes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|process| { process["background_renderer"] == false }),
                    "The headless capacity fixture started an extension or browser UI renderer"
                );
            }
            (
                json!({"cycle":cycle,"phase":if cycle<20 {"warmup"} else {"measured"},
                "fills":[first,second],"invalidated_bytes":0,"opened":opened}),
                identities,
            )
        })
        .catch_unwind()
        .await;
        let closed_a = a.close().await;
        let closed_b = b.close().await;
        let (mut row, identities) = match result {
            Ok(value) => value,
            Err(error) => {
                writeln!(
                    output,
                    "{}",
                    json!({"cycle":cycle,"failed":true,"closed":[closed_a,closed_b]})
                )
                .unwrap();
                server.abort();
                std::panic::resume_unwind(error)
            }
        };
        tokio::time::sleep(Duration::from_millis(
            protocol["idle_interval_ms"].as_u64().unwrap(),
        ))
        .await;
        let (idle, remaining) = snapshot();
        assert!(
            remaining.is_empty(),
            "Owned child process remains after close: {remaining:?}"
        );
        assert!(
            identities
                .iter()
                .all(|(pid, start)| process(*pid).is_none_or(|p| p.start != *start)),
            "A captured native process survived closing"
        );
        row["closed"] = json!([closed_a, closed_b]);
        row["idle"] = idle;
        row["remaining_owned_processes"] = 0.into();
        writeln!(output, "{row}").unwrap();
        output.flush().unwrap();
        if cycle >= 20 {
            measurements.push(json!({"host":row["idle"]["host"]["rss_kib"],"browser":row["opened"]["browser_rss_kib"],"helper":row["opened"]["helper_rss_kib"]}));
        }
        println!("delta memory {label}: cycle {cycle}, all sessions closed");
    }
    server.abort();
    let mut gates = Vec::new();
    for field in ["host", "browser", "helper"] {
        let metric = |row: &Value| row[field].as_u64().unwrap();
        let first = median(measurements[..10].iter().map(metric).collect());
        let last = median(measurements[90..].iter().map(metric).collect());
        let growth = last.saturating_sub(first);
        gates.push(json!({"metric":field,"first_kib":first,"last_kib":last,"growth_kib":growth,
            "limit_kib":protocol["growth_limit_kib"],"passed":growth<=protocol["growth_limit_kib"].as_u64().unwrap()}));
    }
    let summary = json!({"backend":backend,"cache_enabled":enabled,"protocol":protocol,"warmup_cycles":20,
        "measured_cycles":100,"opened_sessions":240,"closed_sessions":240,"gates":gates,
        "host_idle_first_kib":median(measurements[..10].iter().map(|row|row["host"].as_u64().unwrap()).collect()),
        "host_idle_last_kib":median(measurements[90..].iter().map(|row|row["host"].as_u64().unwrap()).collect())});
    fs::write(
        destination.join(format!("{label}-summary.json")),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    assert!(
        summary["gates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|gate| gate["passed"] == true),
        "{summary}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "frozen cache memory acceptance in an exclusive 2 CPU / 4 GiB release fixture"]
async fn native_delta_memory_cdp_off() {
    memory("cdp", false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "frozen cache memory acceptance in an exclusive 2 CPU / 4 GiB release fixture"]
async fn native_delta_memory_cdp_on() {
    memory("cdp", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "frozen cache memory acceptance in an exclusive 2 CPU / 4 GiB release fixture"]
async fn native_delta_memory_bidi_off() {
    memory("bidi", false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "frozen cache memory acceptance in an exclusive 2 CPU / 4 GiB release fixture"]
async fn native_delta_memory_bidi_on() {
    memory("bidi", true).await;
}
