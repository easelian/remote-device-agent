//! The calibration is kept separate from frozen memory sampling. Its page is
//! real, complete native output; codec-only padding is never used here.
use super::*;

async fn capacity_site() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!(
        "http://{}/?p={}",
        listener.local_addr().unwrap(),
        "p".repeat(8000)
    );
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|| async {
            axum::response::Html(include_str!(
                "../../../../tests/fixtures/observation-delta-v1/capacity.html"
            ))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, server)
}

fn projection_bytes(mut entry: Value) -> (usize, Value) {
    let value = &mut entry["agent"];
    let nodes = value["presentation"]
        .as_object_mut()
        .unwrap()
        .remove("nodes")
        .unwrap();
    value["nodes"] = nodes;
    let mut writer = Writer {
        bytes: None,
        length: 0,
        limit: usize::MAX,
    };
    projection(&mut writer, value, None).unwrap();
    (writer.length, entry)
}

async fn sample(fixture: &mut Fixture, tail: usize, ascii: usize, title_ascii: usize) -> Value {
    fixture
        .fixture(&format!(
            "(()=>{{window.capacity({tail},{ascii},{title_ascii});return true}})()"
        ))
        .await;
    let (bytes, entry) = projection_bytes(fixture.read(None, false, true).await);
    let value = &entry["agent"];
    json!({"tail":tail,"ascii":ascii,"title_ascii":title_ascii,"encoded_bytes":bytes,
        "node_count":value["nodes"].as_array().unwrap().len(),
        "coverage":value["coverage"],"page_title_bytes":value["page"]["title"].as_str().unwrap().len(),
        "page_url_bytes":value["page"]["url"].as_str().unwrap().len(),
        "records_complete":value["nodes"].as_array().unwrap().iter().all(|node|node["record_complete"]==true),
        "cacheable":value["presentation"]["cacheable"],"cache":entry["cache"]})
}

async fn calibrate(backend: &'static str) {
    let destination = PathBuf::from(std::env::var_os("RDH_DELTA_EVIDENCE").unwrap());
    fs::create_dir_all(&destination).unwrap();
    let mut output =
        fs::File::create_new(destination.join(format!("{backend}-calibration.jsonl"))).unwrap();
    let (url, server) = capacity_site().await;
    let mut fixture = Fixture::new(backend).await;
    let result = std::panic::AssertUnwindSafe(async {
        fixture.navigate(&url).await;
        let mut low = 0;
        let mut high = 4096;
        while low + 1 < high {
            let tail = (low + high) / 2;
            let result = sample(&mut fixture, tail, 0, 0).await;
            writeln!(output, "{result}").unwrap();
            output.flush().unwrap();
            if result["cacheable"] == true {
                low = tail;
            } else {
                high = tail;
            }
        }
        for tail in low.saturating_sub(2)..=low {
            let measured = sample(&mut fixture, tail, 0, 0).await;
            writeln!(output, "{measured}").unwrap();
            output.flush().unwrap();
            let bytes = measured["encoded_bytes"].as_u64().unwrap() as usize;
            for ascii in 0..=12 {
                for title_ascii in 0..=7 {
                    if bytes + 2 * ascii != MAX_BASELINE + 3 * title_ascii {
                        continue;
                    }
                    let exact = sample(&mut fixture, tail, ascii, title_ascii).await;
                    writeln!(output, "{exact}").unwrap();
                    output.flush().unwrap();
                    if exact["cacheable"] == true && exact["encoded_bytes"] == MAX_BASELINE {
                        let again = sample(&mut fixture, tail, ascii, title_ascii).await;
                        assert_eq!(again["cache"]["encoded_bytes"], 2 * MAX_BASELINE);
                        return exact;
                    }
                }
            }
        }
        panic!(
            "Native page did not fill the frozen capacity; inspect the preserved calibration rows"
        );
    })
    .catch_unwind()
    .await;
    let closed = fixture.close().await;
    server.abort();
    fs::write(
        destination.join(format!("{backend}-calibration-close.json")),
        serde_json::to_vec_pretty(&closed).unwrap(),
    )
    .unwrap();
    let selected = match result {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error),
    };
    fs::write(
        destination.join(format!("{backend}-capacity.json")),
        serde_json::to_vec_pretty(&selected).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit native capacity calibration before freezing RSS fixtures"]
async fn native_delta_capacity_cdp() {
    calibrate("cdp").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit native capacity calibration before freezing RSS fixtures"]
async fn native_delta_capacity_bidi() {
    calibrate("bidi").await;
}

#[cfg(target_os = "linux")]
#[path = "presentation_memory_tests.rs"]
mod memory;

#[path = "presentation_performance_tests.rs"]
mod performance;
