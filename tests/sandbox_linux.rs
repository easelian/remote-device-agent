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

#[test]
fn browser_storage_is_hidden_from_a_sandboxed_shell() {
    // Not under /tmp: the sandbox gives commands a /tmp of their own, and this
    // test is about paths that a command sees as they are on the device.
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "rdh-sandbox-browser-{}",
        rdh_agent::random::hex_token::<8>().unwrap()
    ));
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "browser_storage_worker", "--nocapture"])
        .env("RDH_SANDBOX_BROWSER_ROOT", &base)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(&base);
    assert!(output.status.success(), "{text}");
    if text.contains("BWRAP_UNAVAILABLE") {
        eprintln!("bubblewrap is unavailable; the browser storage check did not run");
    } else {
        assert!(text.contains("BWRAP_VERIFIED"), "{text}");
    }
}

#[test]
fn browser_storage_worker() {
    let Some(base) = std::env::var_os("RDH_SANDBOX_BROWSER_ROOT").map(PathBuf::from) else {
        return;
    };
    // The Agent's own config directory exists. The other directories that the
    // sandbox hides do not: a device that never had them must still run commands.
    std::fs::create_dir_all(base.join("home/.config/remote-device-hub")).unwrap();
    // The config file is kept in a place of the owner's choosing, once outside
    // the shared root and once inside a writable one. Browser sessions keep
    // their profiles beside it.
    for (name, shared_root_holds_config) in [("outside", false), ("inside", true)] {
        let case = base.join(name);
        let custom = case.join("custom");
        let work = case.join("work");
        let browser = custom.join(".rdh-browser");
        std::fs::create_dir_all(browser.join("profile")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let cookies = browser.join("profile/Cookies");
        std::fs::write(&cookies, b"browser-session-cookie").unwrap();
        let config_file = custom.join("device.json");
        let policy_file = custom.join("device.policy.json");
        std::fs::write(&config_file, b"fixture-device-token").unwrap();
        std::fs::write(&policy_file, b"fixture-policy-secret").unwrap();
        let shared = if shared_root_holds_config {
            &case
        } else {
            &work
        };
        let config = DeviceConfig::parse(json!({"gateway_url":"http://localhost:8111", "device_id":"fixture", "token":"x".repeat(32),
            "roots":{"work":shared}, "allow_shell":true,"allow_write":true})).unwrap();
        let policy = Policy::parse(
            &json!({"version":1,"sandbox":{"mode":"bwrap","network":false,"hide_home":false}}),
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
        let files = Arc::new(Files::new(&config.roots, true).unwrap());
        let processes =
            Processes::with_sandbox(&config, files, vec![], None, Arc::new(sandbox)).unwrap();
        let command = format!(
            r#"
            if grep -q browser-session-cookie '{cookies}' 2>/dev/null; then echo COOKIE_READ; else echo COOKIE_HIDDEN; fi
            if ls -A '{browser}' 2>/dev/null | grep -q .; then echo BROWSER_LISTED; else echo BROWSER_EMPTY; fi
            printf planted > '{browser}/planted' 2>/dev/null
            [ ! -s '{config}' ] && [ ! -s '{policy}' ] && echo CONFIG_HIDDEN
            echo COMMAND_RAN
        "#,
            cookies = cookies.display(),
            browser = browser.display(),
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
        assert_eq!(result["status"], "exited", "{name}: {result}");
        assert_eq!(result["exit_code"], 0, "{name}: {result}");
        let output = result["output"].as_str().unwrap();
        for marker in [
            "COOKIE_HIDDEN",
            "BROWSER_EMPTY",
            "CONFIG_HIDDEN",
            "COMMAND_RAN",
        ] {
            assert!(
                output.contains(marker),
                "{name}: missing {marker}: {output}"
            );
        }
        assert!(!output.contains("COOKIE_READ"), "{name}: {output}");
        // Whatever the command wrote went to the empty directory it was shown.
        assert!(!browser.join("planted").exists(), "{name}");
        assert_eq!(std::fs::read(&cookies).unwrap(), b"browser-session-cookie");
        assert!(processes.close(Duration::from_secs(3)));
    }
    println!("BWRAP_VERIFIED");
}
