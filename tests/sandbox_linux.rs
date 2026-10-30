#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use rdh_agent::config::DeviceConfig;
use rdh_agent::files::Files;
use rdh_agent::policy::Policy;
use rdh_agent::processes::Processes;
use rdh_agent::sandbox::Sandbox;
use serde_json::json;

#[test]
fn bubblewrap_uses_real_namespaces_and_enforces_mounts_and_limits() {
    let root = std::env::temp_dir().join(format!(
        "rdh-sandbox-{}",
        rdh_agent::random::hex_token::<8>().unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "bubblewrap_worker", "--nocapture"])
        .env("RDH_SANDBOX_TEST_ROOT", &root)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", "/usr/bin:/bin")
        .env("DISPLAY", "host-display-must-not-leak")
        .env("RDH_EXPLICIT_TEST_VALUE", "explicit-value")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
    assert!(output.status.success(), "{text}");
    if text.contains("BWRAP_UNAVAILABLE") {
        eprintln!("bubblewrap is unavailable; only fail-closed behavior was verified");
    } else {
        assert!(text.contains("BWRAP_VERIFIED"), "{text}");
    }
}

#[test]
fn bubblewrap_worker() {
    let Some(root) = std::env::var_os("RDH_SANDBOX_TEST_ROOT").map(PathBuf::from) else {
        return;
    };
    let home = root.join("home");
    std::fs::write(home.join("secret"), b"home-secret").unwrap();
    std::fs::create_dir_all(home.join(".config/remote-device-hub")).unwrap();
    std::fs::create_dir_all(home.join(".local/state/remote-device-hub")).unwrap();
    std::fs::write(
        home.join(".config/remote-device-hub/secret"),
        b"state-secret",
    )
    .unwrap();
    let readonly = root.join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    let config_file = root.join("device.json");
    let policy_file = root.join("device.policy.json");
    std::fs::write(&config_file, b"fixture-device-token").unwrap();
    std::fs::write(&policy_file, b"fixture-policy-secret").unwrap();
    let config = DeviceConfig::parse(json!({"gateway_url":"http://localhost:8111", "device_id":"fixture", "token":"x".repeat(32),
        "roots":{"work":root,"ro":readonly,"same_rw":readonly}, "allow_shell":true,"allow_write":true})).unwrap();
    let policy = Policy::parse(
        &json!({"version":1,"readonly_roots":["ro"],
        "sandbox":{"mode":"bwrap","network":false,"hide_home":true,"limits":{"open_files":64}}}),
        &config,
    )
    .unwrap();
    let sandbox = match Sandbox::prepare(&config, &policy, Some(&config_file)) {
        Ok(sandbox) => sandbox,
        Err(error) if error.to_string().contains("bubblewrap") => {
            println!("BWRAP_UNAVAILABLE: {error}");
            return;
        }
        Err(error) => panic!("sandbox preparation failed: {error}"),
    };
    assert_eq!(sandbox.report()["network"], false);
    let files = Arc::new(Files::new(&config.roots, true).unwrap());
    let processes = Processes::with_sandbox(
        &config,
        files,
        vec!["RDH_EXPLICIT_TEST_VALUE".into()],
        None,
        Arc::new(sandbox),
    )
    .unwrap();
    let host_network = std::fs::read_link("/proc/self/ns/net")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let command = format!(
        r#"
        [ ! -e "$HOME/secret" ] && echo HOME_HIDDEN
        [ ! -e "$HOME/.config/remote-device-hub/secret" ] && echo STATE_HIDDEN
        [ ! -s '{config}' ] && [ ! -s '{policy}' ] && echo CONFIG_HIDDEN
        [ -z "${{DISPLAY+x}}" ] && echo SESSION_HIDDEN
        [ "$RDH_EXPLICIT_TEST_VALUE" = explicit-value ] && echo EXPLICIT_PRESENT
        printf writable > '{root}/writable' && echo ROOT_WRITABLE
        if printf forbidden > '{root}/readonly/blocked' 2>/dev/null; then echo ROOT_READONLY_FAILED; else echo ROOT_READONLY; fi
        printf 'LIMIT:'; ulimit -n
        printf 'NET:'; /usr/bin/readlink /proc/self/ns/net
    "#,
        root = root.display(),
        config = config_file.display(),
        policy = policy_file.display()
    );
    let result = processes
        .dispatch(
            "start_process",
            &json!({"root":"work","command":command,"wait_seconds":5}),
            "fixture",
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(result["status"], "exited", "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    let output = result["output"].as_str().unwrap();
    for marker in [
        "HOME_HIDDEN",
        "STATE_HIDDEN",
        "CONFIG_HIDDEN",
        "SESSION_HIDDEN",
        "EXPLICIT_PRESENT",
        "ROOT_WRITABLE",
        "ROOT_READONLY",
        "LIMIT:64",
    ] {
        assert!(output.contains(marker), "missing {marker}: {output}");
    }
    assert!(!output.contains("ROOT_READONLY_FAILED"));
    let network = output
        .lines()
        .find_map(|line| line.strip_prefix("NET:"))
        .unwrap();
    assert_ne!(network, host_network);
    assert!(!readonly.join("blocked").exists());
    assert!(root.join("writable").exists());
    assert!(processes.close(Duration::from_secs(3)));
    println!("BWRAP_VERIFIED");
}

#[test]
fn posix_limits_also_apply_without_bubblewrap() {
    let root = std::env::temp_dir().join(format!(
        "rdh-limits-{}",
        rdh_agent::random::hex_token::<8>().unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let config = DeviceConfig::parse(
        json!({"gateway_url":"http://localhost:8111", "device_id":"fixture", "token":"x".repeat(32),
        "roots":{"work":root}, "allow_shell":true}),
    )
    .unwrap();
    let policy = Policy::parse(
        &json!({"version":1,"sandbox":{"mode":"none", "limits":{
        "cpu_seconds":5, "memory_kb":262144, "open_files":64, "file_size_blocks":16}}}),
        &config,
    )
    .unwrap();
    let sandbox = Sandbox::prepare(&config, &policy, None).unwrap();
    assert_eq!(sandbox.report()["limits"]["open_files"], 64);
    let files = Arc::new(Files::new(&config.roots, false).unwrap());
    let processes =
        Processes::with_sandbox(&config, files, vec![], None, Arc::new(sandbox)).unwrap();
    let result = processes
        .dispatch(
            "start_process",
            &json!({"root":"work", "wait_seconds":3,
        "command":"ulimit -t; ulimit -v; ulimit -n; ulimit -f"}),
            "fixture",
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(result["status"], "exited", "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["output"], "5\n262144\n64\n16\n");
    assert!(processes.close(Duration::from_secs(2)));
    std::fs::remove_dir_all(root).unwrap();
}
