#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files")
))]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use rdh_agent::{files::Files, random, searches::Searches};
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn start(manager: &Arc<Searches>, params: Value) -> Result<Value, String> {
    manager.start(
        &params,
        Instant::now() + Duration::from_secs(3),
        &AtomicBool::new(false),
    )
}

fn without_id(mut value: Value) -> Value {
    value.as_object_mut().unwrap().remove("search_id");
    value
}

async fn complete(manager: &Searches, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = manager.get(&json!({"search_id":id})).unwrap();
            if !matches!(result["status"].as_str(), Some("queued" | "running"))
                && manager.quiescent()
            {
                return without_id(result);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native search did not finish")
}

#[tokio::test]
#[ignore = "requires a bundled legacy Node search fixture; run explicitly"]
async fn legacy_and_rust_background_search_contracts_match() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_SEARCH_HELPER")
            .expect("RDH_LEGACY_SEARCH_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let fixture = Fixture(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-legacy-search-{}",
        random::hex_token::<12>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    fs::create_dir(fixture.0.join("deep")).unwrap();
    fs::write(fixture.0.join("Straße"), "needle one\r\nneedle 😀 two\n").unwrap();
    fs::write(fixture.0.join("deep/𐀀Straße"), "\u{feff}needle three\n").unwrap();
    fs::write(
        fixture.0.join("deep/\u{e000}Straße"),
        format!("unrelated\nneedle {}", "😀".repeat(510)),
    )
    .unwrap();
    fs::write(fixture.0.join("invalid"), [0xff]).unwrap();
    symlink(fixture.0.join("Straße"), fixture.0.join("strasse-link")).unwrap();

    let node = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .arg(&helper)
        .arg(&fixture.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("legacy search fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "legacy fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let legacy: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        legacy["retention"],
        json!({"slow_retained":true,"fast_retained":false,"total":16})
    );

    let files = Files::new(
        &BTreeMap::from([("work".into(), fixture.0.to_str().unwrap().into())]),
        false,
    )
    .unwrap();
    let manager = Searches::new(Arc::new(files));
    let invalid = json!({
        "input": start(&manager, Value::Null).unwrap_err(),
        "path_type": start(&manager, json!({"root":"work","path":null,"query":"x"})).unwrap_err(),
        "query_before_pattern": start(&manager, json!({"root":"work","query":"","pattern":0})).unwrap_err(),
        "pattern_before_content": start(&manager, json!({"root":"work","query":"x","pattern":0,"content":0})).unwrap_err(),
        "content": start(&manager, json!({"root":"work","query":"x","content":0})).unwrap_err(),
        "root_before_path": start(&manager, json!({"root":"missing","path":"../x","query":"x"})).unwrap_err(),
        "parent_path": start(&manager, json!({"root":"work","path":"../x","query":"x"})).unwrap_err(),
    });
    let name = start(&manager, json!({"root":"work","query":"strasse"})).unwrap();
    assert!(matches!(
        name["status"].as_str(),
        Some("queued" | "running")
    ));
    let id = name["search_id"].as_str().unwrap();
    let name_result = complete(&manager, id).await;
    let name_page = without_id(
        manager
            .get(&json!({"search_id":id,"offset":1,"limit":1}))
            .unwrap(),
    );
    let page_errors = json!({
        "negative": manager.get(&json!({"search_id":id,"offset":-1})).unwrap_err(),
        "fractional": manager.get(&json!({"search_id":id,"offset":0.5})).unwrap_err(),
        "large": manager.get(&json!({"search_id":id,"offset":1e100})).unwrap_err(),
        "zero_limit": manager.get(&json!({"search_id":id,"limit":0})).unwrap_err(),
        "both_invalid": manager.get(&json!({"search_id":id,"offset":1e100,"limit":0})).unwrap_err(),
    });
    let content = start(
        &manager,
        json!({"root":"work","query":"needle","content":true}),
    )
    .unwrap();
    let content_result = complete(&manager, content["search_id"].as_str().unwrap()).await;
    let empty_name = start(&manager, json!({"root":"work","path":"","query":"strasse"})).unwrap();
    let empty_name_result = complete(&manager, empty_name["search_id"].as_str().unwrap()).await;
    let empty_content = start(
        &manager,
        json!({"root":"work","path":"","query":"needle","content":true}),
    )
    .unwrap();
    let empty_content_result =
        complete(&manager, empty_content["search_id"].as_str().unwrap()).await;
    let nested = start(
        &manager,
        json!({"root":"work","path":"deep/","query":"needle","content":true}),
    )
    .unwrap();
    let nested_result = complete(&manager, nested["search_id"].as_str().unwrap()).await;
    let absent = match start(
        &manager,
        json!({"root":"work","path":"missing","query":"x"}),
    ) {
        Ok(started) => {
            let mut value = complete(&manager, started["search_id"].as_str().unwrap()).await;
            let error = value.as_object_mut().unwrap().remove("error");
            value["has_error"] = json!(
                error
                    .as_ref()
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            );
            value
        }
        Err(error) => json!({"start_error":error}),
    };
    assert!(manager.close(Duration::from_secs(3)).await);
    let closed = start(&manager, json!({"root":"work","query":""})).unwrap_err();
    let actual = json!({
        "invalid": invalid,
        "nameResult": name_result,
        "namePage": name_page,
        "pageErrors": page_errors,
        "contentResult": content_result,
        "emptyNameResult": empty_name_result,
        "emptyContentResult": empty_content_result,
        "nestedResult": nested_result,
        "absent": absent,
        "closed": closed,
    });
    let mut differences = Vec::new();
    for key in [
        "invalid",
        "nameResult",
        "namePage",
        "pageErrors",
        "contentResult",
        "emptyNameResult",
        "emptyContentResult",
        "nestedResult",
        "absent",
        "closed",
    ] {
        if actual[key] != legacy[key] {
            differences.push(format!(
                "{key}: legacy={} native={}",
                legacy[key], actual[key]
            ));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
