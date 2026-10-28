use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rdh_agent::receipts::{Begin, ReceiptError, Receipts};
use serde_json::{Value, json};

const CASES: &[(&str, &str, &str)] = &[
    ("key-order", r#"{"z":1,"a":2}"#, r#"{"a":2,"z":1}"#),
    ("integer-form", r#"{"n":1}"#, r#"{"n":1.0}"#),
    ("negative-zero", r#"{"n":-0.0}"#, r#"{"n":0}"#),
    (
        "rounded-number",
        r#"{"n":9007199254740992}"#,
        r#"{"n":9007199254740993}"#,
    ),
    (
        "distinct-number",
        r#"{"n":9007199254740992}"#,
        r#"{"n":9007199254740994}"#,
    ),
    (
        "u64-rounding",
        r#"{"n":18446744073709551615}"#,
        r#"{"n":18446744073709551614}"#,
    ),
    ("scientific", r#"{"n":1e-7}"#, r#"{"n":0.0000001}"#),
    (
        "numeric-keys",
        r#"{"10":"a","2":"b","x":0}"#,
        r#"{"x":0,"2":"b","10":"a"}"#,
    ),
    ("unicode-keys", "{\"𐀀\":1,\"\":2}", "{\"\":2,\"𐀀\":1}"),
    (
        "nested-objects",
        r#"{"x":[{"b":2,"a":1},null]}"#,
        r#"{"x":[{"a":1,"b":2},null]}"#,
    ),
    ("array-order", r#"{"x":[1,2]}"#, r#"{"x":[2,1]}"#),
    ("number-vs-boolean", r#"{"x":0}"#, r#"{"x":false}"#),
    (
        "unicode-normalization",
        "{\"x\":\"é\"}",
        "{\"x\":\"e\u{301}\"}",
    ),
    ("null-vs-absent", r#"{"x":null}"#, "{}"),
];

fn attempt(result: Result<Begin, ReceiptError>) -> String {
    match result {
        Ok(Begin::New) => "new".into(),
        Ok(Begin::Existing(_)) => "existing".into(),
        Err(error) => format!("{}:{}", error.code, error.state),
    }
}

fn status(receipts: &Receipts, id: &Value, instance: &Value) -> Value {
    let mut result = receipts.status(id, instance).unwrap();
    result.as_object_mut().unwrap().remove("instance_id");
    result
}

#[test]
#[ignore = "requires a bundled legacy Node receipt fixture; run explicitly"]
fn legacy_and_rust_receipt_equivalence_and_lifecycle_match() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_RECEIPTS_HELPER")
            .expect("RDH_LEGACY_RECEIPTS_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let node = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .arg(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let corpus: Vec<_> = CASES
        .iter()
        .map(|(label, left, right)| json!({"label":label,"left":left,"right":right}))
        .collect();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&corpus).unwrap())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("legacy receipt fixture timed out");
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
    let id = json!("a".repeat(32));
    let second_id = json!("b".repeat(32));
    let third_id = json!("c".repeat(32));
    let comparisons: Vec<_> = CASES.iter().map(|(label, left, right)| {
        let receipts = Receipts::new("fixture-token").unwrap();
        let instance = json!(receipts.instance_id);
        let left: Value = serde_json::from_str(left).unwrap();
        let right: Value = serde_json::from_str(right).unwrap();
        assert!(matches!(receipts.begin(&id, &instance, "write_file", &left), Ok(Begin::New)));
        json!({"label":label,"outcome":attempt(receipts.begin(&id,&instance,"write_file",&right))})
    }).collect();
    let receipts = Receipts::with_limits("fixture-token", 2, Duration::from_secs(3600)).unwrap();
    let instance = json!(receipts.instance_id);
    let params = json!({"content":"receipt-fixture-secret"});
    assert!(matches!(
        receipts.begin(&id, &instance, "write_file", &params),
        Ok(Begin::New)
    ));
    assert!(matches!(
        receipts.begin(
            &second_id,
            &instance,
            "start_process",
            &json!({"command":"true"})
        ),
        Ok(Begin::New)
    ));
    let capacity = attempt(receipts.begin(&third_id, &instance, "write_file", &json!({})));
    receipts.complete(
        id.as_str().unwrap(),
        "succeeded",
        &json!({"job_id":"job_1","sha256":"f".repeat(64),
        "size_bytes":12,"exit_code":null,"stdin_closed":true,"output":"receipt-fixture-secret"}),
        None,
    );
    receipts.complete(id.as_str().unwrap(), "unknown", &json!({}), None);
    receipts.complete(
        second_id.as_str().unwrap(),
        "failed",
        &json!({}),
        Some("policy_root_readonly"),
    );
    let settled = status(&receipts, &id, &instance);
    assert!(!settled.to_string().contains("receipt-fixture-secret"));
    let pending = receipts.pending();
    receipts.acknowledge(&id, &json!("different-instance"));
    let after_wrong_ack = receipts.pending();
    receipts.acknowledge(&second_id, &instance);
    let after_second_ack = receipts.pending();
    receipts.acknowledge(&id, &instance);
    let after_all_ack = receipts.pending();
    let replay = attempt(receipts.begin(&id, &instance, "write_file", &params));
    let changed =
        attempt(receipts.begin(&id, &instance, "write_file", &json!({"content":"changed"})));
    let old_instance = status(&receipts, &id, &json!("different-instance"));
    let stale_start =
        attempt(receipts.begin(&id, &json!("different-instance"), "write_file", &json!({})));
    let actual = json!({"comparisons":comparisons,"lifecycle":{"capacity":capacity,"settled":settled,
        "pending":pending,"afterWrongAck":after_wrong_ack,"afterSecondAck":after_second_ack,
        "afterAllAck":after_all_ack,"replay":replay,"changed":changed,"oldInstance":old_instance,"staleStart":stale_start}});
    assert_eq!(actual, legacy);
}
