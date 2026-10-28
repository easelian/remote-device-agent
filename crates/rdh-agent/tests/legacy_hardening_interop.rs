#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rdh_agent::{config::DeviceConfig, policy::Policy, random, rpc::Runtime};
use serde_json::{Value, json};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
#[ignore = "requires a bundled legacy Node hardening fixture; run explicitly"]
async fn legacy_and_rust_hardening_reports_and_revision_stability() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_HARDENING_HELPER")
            .expect("RDH_LEGACY_HARDENING_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "rdh-legacy-hardening-{}",
        random::hex_token::<12>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
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
            panic!("legacy hardening fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "legacy fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(observed["reordered_roots_revisions_differ"], true);
    let mut legacy = observed["report"].clone();
    let config = DeviceConfig {
        gateway_url: "http://localhost".into(),
        device_id: "fixture".into(),
        token: "fixture-token-01234567890123456789".into(),
        roots: BTreeMap::from([("work".into(), fixture.0.to_str().unwrap().into())]),
        allow_shell: true,
        allow_write: true,
        shell: "/bin/sh".into(),
        max_processes: 4,
        max_process_seconds: 10,
    };
    let policy = Policy::parse(
        &json!({"version":1,
        "allowed_operations":["read_file","write_file"], "readonly_roots":["work"],
        "protected_paths":[".env","private/key"]}),
        &config,
    )
    .unwrap();
    let runtime = Runtime::new(config.clone(), policy.clone(), None).unwrap();
    let mut actual = runtime.info()["hardening"].clone();
    let revision = actual["policy_revision"].as_str().unwrap().to_owned();
    assert_eq!(revision, runtime.info()["hardening"]["policy_revision"]);
    for value in [legacy["policy_revision"].as_str().unwrap(), &revision] {
        assert_eq!(value.len(), 64);
        assert!(
            value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
    assert!(runtime.shutdown().await);
    let restarted = Runtime::new(config.clone(), policy, None).unwrap();
    assert_eq!(revision, restarted.info()["hardening"]["policy_revision"]);
    assert!(restarted.shutdown().await);
    let changed = Policy::parse(
        &json!({"version":1,
        "allowed_operations":["read_file","write_file"], "readonly_roots":["work"],
        "protected_paths":[".env","private/other"]}),
        &config,
    )
    .unwrap();
    let changed = Runtime::new(config, changed, None).unwrap();
    assert_ne!(revision, changed.info()["hardening"]["policy_revision"]);
    assert!(changed.shutdown().await);
    // Implementations serialize the revision identity differently. Compare
    // the actual restrictions separately, and check revision stability above.
    legacy.as_object_mut().unwrap().remove("policy_revision");
    actual.as_object_mut().unwrap().remove("policy_revision");
    assert_eq!(actual, legacy);
}
