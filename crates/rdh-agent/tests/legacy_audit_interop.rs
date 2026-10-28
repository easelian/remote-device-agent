#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-audit")
))]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rdh_agent::{
    audit::{AuditLog, AuditRecord},
    config::sha256_hex,
    random,
};
use serde_json::{Value, json};

const SECRET: &str = "AUDIT_FIXTURE_SECRET";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rdh-legacy-audit-{}",
            random::hex_token::<12>().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn node(&self, helper: &Path, label: &str, count: usize, max_bytes: u64, required: bool) {
        let node = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| "node".into());
        let mut child = Command::new(node)
            .arg(helper)
            .arg(&self.0)
            .arg(label)
            .arg(count.to_string())
            .arg(max_bytes.to_string())
            .arg(required.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("legacy audit fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "legacy fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stats: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            stats,
            json!({"written":count,"dropped":0,"last_error":null})
        );
    }

    fn native(&self, count: usize, max_bytes: u64) {
        let log = AuditLog::open(&self.0, "fixture", max_bytes, 3, true).unwrap();
        for index in 0..count {
            log.record(AuditRecord {
                id: Some(format!("rust-{index}")),
                operation: "write_file".into(),
                status: "succeeded".into(),
                duration_ms: 7,
                params: json!({"root":"work","path":"fixture.txt","content":SECRET}),
                result: Some(json!({"size_bytes":20,"output":SECRET})),
                error: None,
            })
            .unwrap();
        }
        assert_eq!(
            log.stats(),
            json!({"written":count,"dropped":0,"last_error":null})
        );
    }

    fn records(&self) -> Vec<(Vec<u8>, Value)> {
        let mut result = Vec::new();
        for index in (0..20).rev() {
            let name = if index == 0 {
                "fixture.jsonl".into()
            } else {
                format!("fixture.jsonl.{index}")
            };
            let path = self.0.join(name);
            if !path.exists() {
                continue;
            }
            let metadata = fs::metadata(&path).unwrap();
            assert!(metadata.is_file());
            assert_eq!(metadata.mode() & 0o077, 0);
            assert_eq!(metadata.nlink(), 1);
            let bytes = fs::read(path).unwrap();
            assert!(
                !bytes
                    .windows(SECRET.len())
                    .any(|window| window == SECRET.as_bytes())
            );
            assert!(bytes.is_empty() || bytes.last() == Some(&b'\n'));
            for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                if !line.is_empty() {
                    result.push((
                        line.to_vec(),
                        serde_json::from_slice::<Value>(line).unwrap(),
                    ));
                }
            }
        }
        for pair in result.windows(2) {
            let (previous_line, previous) = &pair[0];
            let (_, current) = &pair[1];
            // Labels identify the three writer runs even if their random
            // four-byte boot identifiers happen to collide.
            let label = |value: &Value| {
                value["id"]
                    .as_str()
                    .unwrap()
                    .rsplit_once('-')
                    .unwrap()
                    .0
                    .to_owned()
            };
            if label(previous) == label(current) {
                assert_eq!(previous["boot"], current["boot"]);
                assert_eq!(
                    current["seq"].as_u64(),
                    previous["seq"].as_u64().map(|n| n + 1)
                );
                assert_eq!(current["prev"], sha256_hex(previous_line)[..16]);
            } else {
                assert_eq!(current["seq"], 1);
                assert_eq!(current["prev"], "0000000000000000");
            }
        }
        result
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires a bundled legacy Node audit fixture; run explicitly"]
fn legacy_native_and_legacy_audit_retention_interoperate() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_AUDIT_HELPER")
            .expect("RDH_LEGACY_AUDIT_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());

    let retention = Fixture::new();
    let max_bytes = 5 * 1024 * 1024;
    retention.node(&helper, "node-a", 999, max_bytes, false);
    retention.native(3, max_bytes);
    retention.node(&helper, "node-b", 2, max_bytes, true);
    let records = retention.records();
    assert_eq!(records.len(), 1000);
    assert_eq!(records.first().unwrap().1["id"], "node-a-4");
    let ids: Vec<_> = records
        .iter()
        .map(|(_, record)| record["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        &ids[995..],
        ["rust-0", "rust-1", "rust-2", "node-b-0", "node-b-1"]
    );

    let rotation = Fixture::new();
    rotation.node(&helper, "node-a", 9, 4096, false);
    rotation.native(8, 4096);
    rotation.node(&helper, "node-b", 9, 4096, true);
    let records = rotation.records();
    assert_eq!(records.len(), 26);
    assert!(rotation.0.join("fixture.jsonl.1").is_file());
    assert_eq!(records.first().unwrap().1["id"], "node-a-0");
    assert_eq!(records.last().unwrap().1["id"], "node-b-8");
}
