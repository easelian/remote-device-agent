#![cfg(all(windows, feature = "experimental-windows-install"))]

use rdh_agent::{config::sha256_hex, install_lock, profile_lock::ProfileLocks, random, storage};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[path = "support/native_candidate.rs"]
mod native_candidate;
#[path = "support/windows.rs"]
mod windows;
#[path = "support/windows_tasks.rs"]
mod windows_tasks;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let running = self.0.join("install/.running");
        if !running.exists()
            || fs::read_dir(running).is_ok_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir_all(&self.0);
        } else {
            eprintln!(
                "Retaining Windows installation fixture with running markers: {}",
                self.0.display()
            );
        }
    }
}
fn selected(prefix: &Path, name: &str) -> String {
    install_lock::pointer(prefix, name).unwrap()
}
fn binary(prefix: &Path, version: &str) -> PathBuf {
    prefix.join("versions").join(version).join("rdh.exe")
}
fn set_pointer(prefix: &Path, name: &str, version: &str) {
    storage::write_private_replace(
        &prefix.join(format!("{name}.json")),
        &serde_json::to_vec(&json!({
            "schema":1,"kind":"native-version","version":version
        }))
        .unwrap(),
    )
    .unwrap();
}
fn interrupted(prefix: &Path, old: &str, previous: &str, next: &str) {
    storage::write_private_new(
        &prefix.join(install_lock::PENDING),
        &serde_json::to_vec(&json!({
            "schema":1,"old_current":old,"old_previous":previous,"new_current":next
        }))
        .unwrap(),
    )
    .unwrap();
    set_pointer(prefix, "previous", old);
    set_pointer(prefix, "current", next);
}
async fn run(executable: &Path, root: &Path, args: &[&str]) -> std::process::Output {
    let id = random::hex_token::<8>().unwrap();
    let stdout = root.join(format!("command-{id}.out"));
    let stderr = root.join(format!("command-{id}.err"));
    let mut child = windows::command(executable, root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;
    if result.is_err() {
        let _ = child.kill().await;
        child.wait().await.unwrap();
    }
    assert!(fs::metadata(&stdout).unwrap().len() < 65536);
    assert!(fs::metadata(&stderr).unwrap().len() < 65536);
    let stdout_bytes = fs::read(&stdout).unwrap();
    let stderr_bytes = fs::read(&stderr).unwrap();
    fs::remove_file(stdout).unwrap();
    fs::remove_file(stderr).unwrap();
    std::process::Output {
        status: result
            .unwrap_or_else(|_| {
                panic!(
                    "Command timed out: {args:?}\n{}\n{}",
                    String::from_utf8_lossy(&stdout_bytes),
                    String::from_utf8_lossy(&stderr_bytes)
                )
            })
            .unwrap(),
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }
}
fn success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_install_upgrade_rollback_and_crash_recovery_preserve_identity() {
    let (next, candidate) = native_candidate::load();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "rdh-windows-install-{}",
        random::hex_token::<10>().unwrap()
    )));
    windows::private_directory(&fixture.0);
    // Canonical config names match the application's Windows path discovery.
    let root = fixture.0.canonicalize().unwrap();
    let work = root.join("work");
    windows::private_directory(&work);
    fs::write(work.join("keep.txt"), b"user data survives native upgrades").unwrap();
    let config = root.join("existing device.json");
    let token = "private-pairing-must-remain-unchanged";
    storage::write_private_new(&config, &serde_json::to_vec(&json!({
        "gateway_url":"http://127.0.0.1:9", "device_id":"windows-install-fixture", "token":token,
        "roots":{"work":work}, "allow_shell":true, "allow_write":true
    })).unwrap()).unwrap();
    let original_config = fs::read(&config).unwrap();
    let prefix = root.join("install");
    let initial = Path::new(env!("CARGO_BIN_EXE_rdh"));
    let prepare = [
        "install-native",
        "--prefix",
        prefix.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ];
    let lock = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    assert!(!run(initial, &root, &prepare).await.status.success());
    assert!(!prefix.exists());
    drop(lock);
    let alias = root.join("config-hardlink.json");
    fs::hard_link(&config, &alias).unwrap();
    assert!(!run(initial, &root, &prepare).await.status.success());
    assert!(!prefix.exists());
    fs::remove_file(alias).unwrap();
    let policy = config.with_extension("policy.json");
    storage::write_private_new(&policy, br#"{"version":999}"#).unwrap();
    assert!(!run(initial, &root, &prepare).await.status.success());
    assert!(!prefix.exists());
    fs::remove_file(policy).unwrap();
    let output = success(run(initial, &root, &prepare).await);
    assert!(!output.contains(token));
    let current = env!("CARGO_PKG_VERSION");
    let old_binary = binary(&prefix, current);
    assert!(output.contains(old_binary.to_str().unwrap()));
    assert_eq!(selected(&prefix, "current"), current);
    assert!(!prefix.join("current").exists());
    assert!(!run(initial, &root, &prepare).await.status.success());
    assert_eq!(fs::read(&config).unwrap(), original_config);
    assert_eq!(
        success(run(&old_binary, &root, &["upgrade", "--list-profiles"]).await).trim(),
        config.to_str().unwrap()
    );
    let install_id = fs::read(prefix.join("install-id")).unwrap();
    let original_binary = sha256_hex(&fs::read(&old_binary).unwrap());

    let candidate_path = root.join("candidate.exe");
    fs::write(&candidate_path, &candidate).unwrap();
    let platform = if cfg!(target_arch = "x86_64") {
        "win32-x64"
    } else {
        "win32-arm64"
    };
    let mut channel = json!({"schema":2,"channel":"stable","version":next,"platforms":{
        (platform):{"format":"native-binary","url":"candidate.exe","sha256":sha256_hex(&candidate),"bytes":candidate.len()}
    }});
    let index = root.join("channel.json");
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();
    let upgrade = [
        "upgrade",
        "--index",
        index.to_str().unwrap(),
        "--no-restart",
    ];
    success(
        run(
            &old_binary,
            &root,
            &["upgrade", "--index", index.to_str().unwrap(), "--check"],
        )
        .await,
    );
    assert!(!prefix.join("previous.json").exists());
    channel["platforms"][platform]["sha256"] = json!("0".repeat(64));
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();
    assert!(!run(&old_binary, &root, &upgrade).await.status.success());
    assert_eq!(selected(&prefix, "current"), current);
    assert!(!prefix.join(install_lock::PENDING).exists());
    let bad = b"not a PE executable";
    fs::write(&candidate_path, bad).unwrap();
    channel["platforms"][platform]["sha256"] = json!(sha256_hex(bad));
    channel["platforms"][platform]["bytes"] = json!(bad.len());
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();
    assert!(!run(&old_binary, &root, &upgrade).await.status.success());
    assert!(!binary(&prefix, &next).exists());
    fs::write(&candidate_path, &candidate).unwrap();
    channel["platforms"][platform]["sha256"] = json!(sha256_hex(&candidate));
    channel["platforms"][platform]["bytes"] = json!(candidate.len());
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();

    storage::write_private_replace(&config, b"{broken").unwrap();
    let invalid = run(&old_binary, &root, &upgrade).await;
    assert!(!invalid.status.success());
    assert!(!String::from_utf8_lossy(&invalid.stderr).contains(token));
    assert_eq!(selected(&prefix, "current"), current);
    assert_eq!(
        sha256_hex(&fs::read(binary(&prefix, &next)).unwrap()),
        sha256_hex(&candidate)
    );
    storage::write_private_replace(&config, &original_config).unwrap();
    let held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    assert!(!run(&old_binary, &root, &upgrade).await.status.success());
    drop(held);
    success(run(&old_binary, &root, &upgrade).await);
    assert_eq!(selected(&prefix, "current"), next);
    assert_eq!(selected(&prefix, "previous"), current);
    let next_binary = binary(&prefix, &next);
    let pairing = [
        "--server",
        "http://127.0.0.1:9",
        "--config",
        config.to_str().unwrap(),
        "--pair-only",
    ];
    assert!(!run(&old_binary, &root, &pairing).await.status.success());
    assert!(
        !run(&old_binary, &root, &["rollback", "--no-restart"])
            .await
            .status
            .success()
    );
    success(run(&next_binary, &root, &pairing).await);
    success(run(&next_binary, &root, &["rollback", "--no-restart"]).await);
    assert_eq!(selected(&prefix, "current"), current);
    assert_eq!(selected(&prefix, "previous"), next);

    // Reconstruct each durable boundary: journal only, previous changed, and
    // both selectors changed. The actual CLI must recover all three states.
    for phase in 0..3 {
        storage::write_private_new(
            &prefix.join(install_lock::PENDING),
            &serde_json::to_vec(&json!({
                "schema":1,"old_current":current,"old_previous":next,"new_current":next
            }))
            .unwrap(),
        )
        .unwrap();
        if phase >= 1 {
            set_pointer(&prefix, "previous", current);
        }
        if phase >= 2 {
            set_pointer(&prefix, "current", &next);
        }
        assert!(!run(&old_binary, &root, &pairing).await.status.success());
        assert!(!run(&next_binary, &root, &pairing).await.status.success());
        success(run(&next_binary, &root, &["upgrade", "--recover"]).await);
        assert_eq!(selected(&prefix, "current"), current);
        assert_eq!(selected(&prefix, "previous"), next);
        assert!(!prefix.join(install_lock::PENDING).exists());
    }

    // A real installed runtime owns the kernel installation lock. Forced
    // termination deliberately leaves recovery markers; it is not clean exit.
    for recover_without_journal in [false, true] {
        let agent_log = root.join("agent.err");
        let mut agent = windows::command(&old_binary, &root)
            .args([
                "--server",
                "http://127.0.0.1:9",
                "--config",
                config.to_str().unwrap(),
                "--no-browser",
            ])
            .stdout(Stdio::null())
            .stderr(fs::File::create(&agent_log).unwrap())
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                assert!(
                    agent.try_wait().unwrap().is_none(),
                    "Agent exited: {}",
                    fs::read_to_string(&agent_log).unwrap()
                );
                if fs::read_dir(prefix.join(".running"))
                    .is_ok_and(|mut entries| entries.next().is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(!run(&old_binary, &root, &upgrade).await.status.success());
        let mut recover_offline = upgrade.to_vec();
        recover_offline.push("--recover-lock");
        assert!(
            !run(&old_binary, &root, &recover_offline)
                .await
                .status
                .success()
        );
        let live = install_lock::ManagerLock::acquire_with_stale_recovery(
            &prefix,
            &old_binary,
            false,
            true,
        );
        assert!(live.is_err());
        agent.kill().await.unwrap();
        agent.wait().await.unwrap();
        assert!(!run(&old_binary, &root, &upgrade).await.status.success());
        if recover_without_journal {
            // An operator may have stopped the scheduled Task before invoking
            // upgrade. Its terminated Agent can leave locks without any pending
            // version switch; explicit offline recovery must handle that state.
            assert!(!prefix.join(install_lock::PENDING).exists());
            success(run(&old_binary, &root, &recover_offline).await);
            assert_eq!(selected(&prefix, "current"), next);
            success(run(&next_binary, &root, &["rollback", "--no-restart"]).await);
        } else {
            interrupted(&prefix, current, &next, &next);
            assert!(!run(&next_binary, &root, &pairing).await.status.success());
            assert!(
                !run(&next_binary, &root, &["upgrade", "--recover"])
                    .await
                    .status
                    .success()
            );
            success(
                run(
                    &next_binary,
                    &root,
                    &["upgrade", "--recover", "--recover-lock"],
                )
                .await,
            );
        }
        assert_eq!(selected(&prefix, "current"), current);
        assert_eq!(selected(&prefix, "previous"), next);
        assert!(!prefix.join(install_lock::PENDING).exists());
        assert!(!PathBuf::from(format!("{}.node-lock", config.display())).exists());
        assert_eq!(fs::read_dir(prefix.join(".running")).unwrap().count(), 0);
    }

    // Recovery must retain a record with unrelated pointer state.
    interrupted(&prefix, current, &next, &next);
    set_pointer(&prefix, "current", "9.9.9");
    assert!(
        !run(&next_binary, &root, &["upgrade", "--recover"])
            .await
            .status
            .success()
    );
    assert!(prefix.join(install_lock::PENDING).exists());
    assert_eq!(selected(&prefix, "current"), "9.9.9");
    set_pointer(&prefix, "current", &next);
    success(run(&next_binary, &root, &["upgrade", "--recover"]).await);
    success(run(&old_binary, &root, &pairing).await);
    assert_eq!(fs::read(&config).unwrap(), original_config);
    assert_eq!(fs::read(prefix.join("install-id")).unwrap(), install_id);
    assert_eq!(sha256_hex(&fs::read(&old_binary).unwrap()), original_binary);
    assert_eq!(
        fs::read(work.join("keep.txt")).unwrap(),
        b"user data survives native upgrades"
    );
    let profile: Value = serde_json::from_slice(
        &fs::read(
            fs::read_dir(prefix.join(".profiles"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(profile["config"], config.to_str().unwrap());
    assert!(!serde_json::to_string(&profile).unwrap().contains(token));
    let held = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    assert!(
        !run(
            &old_binary,
            &root,
            &["upgrade", "--forget-profile", config.to_str().unwrap()]
        )
        .await
        .status
        .success()
    );
    drop(held);
    success(
        run(
            &old_binary,
            &root,
            &["upgrade", "--forget-profile", config.to_str().unwrap()],
        )
        .await,
    );
    assert_eq!(fs::read_dir(prefix.join(".profiles")).unwrap().count(), 0);
    assert_eq!(fs::read(&config).unwrap(), original_config);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "registers a uniquely named real Windows logon task and changes its native executable target"]
async fn managed_windows_task_targets_follow_offline_update_and_recovery() {
    let (next, candidate) = native_candidate::load();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "rdh task install-{}",
        random::hex_token::<10>().unwrap()
    )));
    windows::private_directory(&fixture.0);
    let root = fixture.0.canonicalize().unwrap();
    let config = root.join("device with spaces.json");
    storage::write_private_new(&config, &serde_json::to_vec(&json!({
        "gateway_url":"http://127.0.0.1:9", "device_id":"native-task-install", "token":"unchanged-task-fixture-token",
        "roots":{"work":root}, "allow_shell":true, "allow_write":true
    })).unwrap()).unwrap();
    let prefix = root.join("install");
    success(
        run(
            Path::new(env!("CARGO_BIN_EXE_rdh")),
            &root,
            &[
                "install-native",
                "--prefix",
                prefix.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
            ],
        )
        .await,
    );
    let original = env!("CARGO_PKG_VERSION");
    let old_binary = binary(&prefix, original);
    let next_binary = binary(&prefix, &next);
    let local = |path: &Path| {
        path.to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap_or(path.to_str().unwrap())
            .to_owned()
    };
    let old_path = local(&old_binary);
    let next_path = local(&next_binary);
    let directory = local(&root);
    // The chosen fixture paths contain spaces but no quotes or trailing
    // slash, so their exact CRT quoting does not depend on product helpers.
    let arguments = format!(
        "\"--config\" \"{}\" \"--server\" \"http://127.0.0.1:9\" \"--no-browser\" \"--recover-lock\"",
        local(&config)
    );
    let sid = windows::current_sid();
    let task = windows_tasks::Task::create(&sid, &old_path, &arguments, &directory);
    success(
        run(
            &old_binary,
            &root,
            &[
                "startup",
                "bind",
                "user",
                &task.name,
                "--config",
                config.to_str().unwrap(),
            ],
        )
        .await,
    );
    let sidecar = PathBuf::from(format!("{}.startup.json", config.display()));
    let sidecar_before = fs::read(&sidecar).unwrap();
    let config_before = fs::read(&config).unwrap();
    let security = task.security();
    fs::write(root.join("candidate.exe"), &candidate).unwrap();
    let platform = if cfg!(target_arch = "x86_64") {
        "win32-x64"
    } else {
        "win32-arm64"
    };
    let index = root.join("channel.json");
    fs::write(&index, serde_json::to_vec(&json!({"schema":2,"channel":"stable","version":next,"platforms":{
        (platform):{"format":"native-binary","url":"candidate.exe","sha256":sha256_hex(&candidate),"bytes":candidate.len()}
    }})).unwrap()).unwrap();
    let upgrade = [
        "upgrade",
        "--index",
        index.to_str().unwrap(),
        "--no-restart",
    ];
    for enabled in [false, true] {
        task.set_enabled(enabled);
        let before = task.inspect();
        success(run(&old_binary, &root, &upgrade).await);
        let after = task.inspect();
        assert_eq!(after.0, next_path);
        assert_eq!(
            (after.1, after.2, after.3, after.4),
            (arguments.clone(), directory.clone(), enabled, before.4)
        );
        assert_eq!(task.security(), security);
        success(run(&next_binary, &root, &["rollback", "--no-restart"]).await);
        assert_eq!(task.inspect(), before);
        assert_eq!(fs::read(&sidecar).unwrap(), sidecar_before);
        assert_eq!(fs::read(&config).unwrap(), config_before);
    }

    // Reconstruct a durable interrupted record with the actual task identity.
    // A changed external startup setting must prevent automatic restoration.
    let target = json!({"config":config,"name":task.name,"sid":sid,"arguments":arguments,"directory":directory,
        "old_executable":old_path,"new_executable":next_path,"fingerprint":task.fingerprint()});
    storage::write_private_new(&prefix.join(install_lock::PENDING), &serde_json::to_vec(&json!({
        "schema":1,"old_current":original,"new_current":next,"old_previous":next,"tasks":[target]
    })).unwrap()).unwrap();
    task.set_enabled(false);
    assert!(
        !run(&old_binary, &root, &["upgrade", "--recover"])
            .await
            .status
            .success()
    );
    assert!(prefix.join(install_lock::PENDING).is_file());
    assert!(!task.inspect().3);
    task.set_enabled(true);
    success(run(&old_binary, &root, &["upgrade", "--recover"]).await);
    assert!(!prefix.join(install_lock::PENDING).exists());
    assert_eq!(task.inspect().0, old_path);
    assert!(task.inspect().3);
    assert_eq!(task.security(), security);
    assert_eq!(fs::read(&sidecar).unwrap(), sidecar_before);
    assert_eq!(fs::read(&config).unwrap(), config_before);
    task.remove();
}
