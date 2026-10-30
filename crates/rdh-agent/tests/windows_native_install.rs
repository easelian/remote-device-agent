#![cfg(all(windows, feature = "experimental-windows-install"))]

#[path = "support/initial_binary.rs"]
mod initial_binary;

use rdh_agent::{
    config::{local_roots, sha256_hex},
    install_lock,
    profile_lock::ProfileLocks,
    random, storage,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        QueryFullProcessImageNameW, WaitForSingleObject,
    },
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
        if std::thread::panicking() {
            eprintln!(
                "Retaining failed Windows installation fixture: {}",
                self.0.display()
            );
            return;
        }
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

async fn aliases(prefix: &Path, root: &Path, expected: &str) {
    for name in rdh_agent::native_launcher::COMMAND_NAMES {
        let alias = prefix.join(format!("{name}.exe"));
        assert_eq!(
            success(run(&alias, root, &["--version"]).await).trim(),
            expected
        );
    }
}

async fn running_task_agent(prefix: &Path, expected: &Path, config: &Path) -> OwnedHandle {
    let expected = expected.canonicalize().unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            // Private writes publish a final JSON name only after flushing
            // their temporary file. Do not observe that staging file.
            let markers: Vec<_> = fs::read_dir(prefix.join(".running"))
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|value| value == "json")
                })
                .collect();
            assert!(markers.len() <= 1, "Only one fixture Agent may be running");
            if let Some(marker) = markers.first() {
                let record: Value =
                    serde_json::from_slice(&fs::read(marker.path()).unwrap()).unwrap();
                assert_eq!(
                    Path::new(record["config"].as_str().unwrap()),
                    config.canonicalize().unwrap(),
                    "New running records use the same canonical spelling as the profile catalog"
                );
                let pid = u32::try_from(record["pid"].as_u64().unwrap()).unwrap();
                let raw = unsafe {
                    OpenProcess(
                        PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                        0,
                        pid,
                    )
                };
                assert!(
                    !raw.is_null(),
                    "Cannot inspect Task Agent: {}",
                    std::io::Error::last_os_error()
                );
                let process = unsafe { OwnedHandle::from_raw_handle(raw) };
                assert_eq!(
                    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) },
                    WAIT_TIMEOUT
                );
                let mut image = vec![0u16; 32768];
                let mut size = image.len() as u32;
                assert_ne!(
                    unsafe {
                        QueryFullProcessImageNameW(
                            process.as_raw_handle(),
                            0,
                            image.as_mut_ptr(),
                            &mut size,
                        )
                    },
                    0
                );
                let actual = PathBuf::from(String::from_utf16(&image[..size as usize]).unwrap());
                assert_eq!(actual.canonicalize().unwrap(), expected);
                return process;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Task did not start the selected installed Agent")
}

fn stopped_task_agent(process: &OwnedHandle) {
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle(), 5000) },
        WAIT_OBJECT_0
    );
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
        "roots":local_roots(&[format!("work={}", work.display())]).unwrap(),
        "allow_shell":true, "allow_write":true
    })).unwrap()).unwrap();
    let original_config = fs::read(&config).unwrap();
    let prefix = root.join("install");
    let initial = initial_binary::path();
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
    let command_entry = prefix.join("rdh.exe");
    aliases(&prefix, &root, current).await;
    let invalid_argument = "--unknown-argument=two words ' \" 雪\\";
    let invalid = run(&command_entry, &root, &[invalid_argument]).await;
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains(invalid_argument));
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
    success(run(&command_entry, &root, &upgrade).await);
    assert_eq!(selected(&prefix, "current"), next);
    aliases(&prefix, &root, &next).await;
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
    success(
        run(
            &prefix.join("remote-device-agent.exe"),
            &root,
            &["rollback", "--no-restart"],
        )
        .await,
    );
    assert_eq!(selected(&prefix, "current"), current);
    aliases(&prefix, &root, current).await;
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
        success(
            run(
                &prefix.join("remote-device-hub.exe"),
                &root,
                &["upgrade", "--recover"],
            )
            .await,
        );
        assert_eq!(selected(&prefix, "current"), current);
        assert_eq!(selected(&prefix, "previous"), next);
        assert!(!prefix.join(install_lock::PENDING).exists());
    }

    // A real installed runtime owns the kernel installation lock. Forced
    // termination deliberately leaves recovery markers; it is not clean exit.
    for recover_without_journal in [false, true] {
        let agent_log = root.join("agent.err");
        let connection_log = root.join("agent.out");
        let mut agent = windows::command(&old_binary, &root)
            .args([
                "--server",
                "http://127.0.0.1:9",
                "--config",
                config.to_str().unwrap(),
                "--no-browser",
            ])
            .stdout(fs::File::create(&connection_log).unwrap())
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
                // The connection message follows completed marker publication;
                // a private staging file alone is not a running Agent.
                if fs::read_to_string(&connection_log)
                    .unwrap()
                    .contains("Connecting to the Hub.")
                {
                    let markers: Vec<_> = fs::read_dir(prefix.join(".running"))
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .collect();
                    assert_eq!(markers.len(), 1);
                    assert_eq!(markers[0].extension().unwrap(), "json");
                    let record: Value =
                        serde_json::from_slice(&storage::read_private(&markers[0]).unwrap())
                            .unwrap();
                    assert_eq!(record["pid"], agent.id().unwrap());
                    assert_eq!(
                        Path::new(record["config"].as_str().unwrap())
                            .canonicalize()
                            .unwrap(),
                        config.canonicalize().unwrap()
                    );
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
        assert!(!PathBuf::from(format!("{}.rdh-lock", config.display())).exists());
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
        "gateway_url":"http://127.0.0.1:9", "device_id":"native-task-install", "token":"unchanged-task-fixture-token-only",
        "roots":local_roots(&[format!("work={}", root.display())]).unwrap(),
        "allow_shell":true, "allow_write":true
    })).unwrap()).unwrap();
    // The Task inherits its user's environment; keep this startup fixture
    // from writing audit data into the runner's actual profile.
    storage::write_private_new(
        &config.with_extension("policy.json"),
        br#"{"version":1,"audit":{"enabled":false}}"#,
    )
    .unwrap();
    let prefix = root.join("install");
    success(
        run(
            initial_binary::path(),
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
    let mut task = windows_tasks::Task::create(&sid, &old_path, &arguments, &directory);
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
    // Exercise the actual Task action at all three versions. Task Stop may
    // terminate the Agent; it is not evidence of a graceful shutdown. Explicit
    // offline recovery must verify dead owners before replacing its target.
    task.start();
    let first = running_task_agent(&prefix, &old_binary, &config).await;
    assert!(!run(&old_binary, &root, &upgrade).await.status.success());
    task.stop();
    stopped_task_agent(&first);
    // Older Agents could persist the Task action's ordinary Win32 path,
    // although the profile catalog already used an extended canonical path.
    // Recovery must resolve that existing metadata, as well as producing
    // canonical metadata for newly started Agents.
    let markers: Vec<_> = fs::read_dir(prefix.join(".running"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(markers.len(), 1);
    let mut record: Value =
        serde_json::from_slice(&storage::read_private(&markers[0]).unwrap()).unwrap();
    record["config"] = local(&config).into();
    storage::write_private_replace(&markers[0], &serde_json::to_vec(&record).unwrap()).unwrap();
    let mut recover_update = upgrade.to_vec();
    recover_update.push("--recover-lock");
    success(run(&old_binary, &root, &recover_update).await);
    assert_eq!(task.inspect().0, next_path);
    task.start();
    let second = running_task_agent(&prefix, &next_binary, &config).await;
    assert!(
        !run(&next_binary, &root, &["rollback", "--no-restart"])
            .await
            .status
            .success()
    );
    task.stop();
    stopped_task_agent(&second);
    success(
        run(
            &next_binary,
            &root,
            &["rollback", "--no-restart", "--recover-lock"],
        )
        .await,
    );
    task.start();
    let restored = running_task_agent(&prefix, &old_binary, &config).await;
    task.stop();
    stopped_task_agent(&restored);
    // Final fixture cleanup uses the same explicit dead-owner recovery as
    // offline CLI updates, without switching the selected version again.
    drop(
        install_lock::ManagerLock::acquire_with_stale_recovery(&prefix, &old_binary, false, true)
            .unwrap(),
    );
    assert_eq!(selected(&prefix, "current"), original);
    assert_eq!(fs::read(&config).unwrap(), config_before);
    assert_eq!(fs::read(&sidecar).unwrap(), sidecar_before);
    assert_eq!(task.security(), security);
    task.remove();
}
