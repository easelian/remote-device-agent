use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[test]
#[ignore = "requires a bundled legacy Node system fixture; run explicitly"]
fn legacy_and_rust_hostnames_and_releases_match() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_SYSTEM_HELPER")
            .expect("RDH_LEGACY_SYSTEM_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let node = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .arg(helper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("legacy system fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "legacy system fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let legacy: Value = serde_json::from_slice(&output.stdout).unwrap();
    let native = rdh_agent::system::info();
    assert_eq!(
        json!({"os": native.os, "hostname": native.hostname, "release": native.release}),
        legacy
    );
}
