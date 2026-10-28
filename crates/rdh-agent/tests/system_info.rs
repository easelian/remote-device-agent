#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[test]
fn native_identity_ignores_computername_environment() {
    let info = rdh_agent::system::info();
    let expected = json!({"os":info.os,"hostname":info.hostname,"release":info.release});
    if std::env::var_os("RDH_TEST_SYSTEM_INFO_CHILD").is_some() {
        println!("RDH_SYSTEM_INFO={expected}");
        return;
    }
    assert_eq!(info.os, "Windows");
    assert!(!info.hostname.is_empty());
    assert!(!info.release.is_empty());
    let fake = format!(
        "rdh-override-{}",
        rdh_agent::random::hex_token::<8>().unwrap()
    );
    assert_ne!(info.hostname, fake);
    // Change only the child's environment, without racing other test threads.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_identity_ignores_computername_environment",
            "--nocapture",
        ])
        .env("RDH_TEST_SYSTEM_INFO_CHILD", "1")
        .env("COMPUTERNAME", fake)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native system info child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "native system info child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let report = text
        .lines()
        .find_map(|line| line.strip_prefix("RDH_SYSTEM_INFO="))
        .expect("child did not report native system information");
    let report: Value = serde_json::from_str(report).unwrap();
    assert_eq!(report, expected);
}
