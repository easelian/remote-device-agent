#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]

#[path = "support/initial_binary.rs"]
mod initial_binary;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

use rdh_agent::install_lock::ManagerLock;
use rdh_agent::{random, storage};
use serde_json::json;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn diagnostics(log: &Path) -> String {
    let mut bytes = Vec::new();
    fs::File::open(log)
        .unwrap()
        .take(65537)
        .read_to_end(&mut bytes)
        .unwrap();
    let truncated = bytes.len() > 65536;
    bytes.truncate(65536);
    format!(
        "{}{}",
        String::from_utf8_lossy(&bytes),
        if truncated {
            "\n[fixture log truncated]"
        } else {
            ""
        }
    )
}

async fn wait_marker(
    directory: &Path,
    previous: Option<&Path>,
    child: &mut tokio::process::Child,
    log: &Path,
) -> PathBuf {
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "Fixture Agent exited before publishing its marker: {status}\n{}",
                    diagnostics(log)
                );
            }
            if let Ok(entries) = fs::read_dir(directory) {
                if let Some(entry) = entries.filter_map(|entry| entry.ok()).find(|entry| {
                    entry.path().extension().is_some_and(|ext| ext == "json")
                        && previous != Some(entry.path().as_path())
                }) {
                    return entry.path();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match result {
        Ok(path) => path,
        Err(_) => {
            // Stop and reap only this fixture process before its tree is removed.
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!(
                "Fixture Agent did not publish its marker after {:?}\n{}",
                started.elapsed(),
                diagnostics(log)
            );
        }
    }
}

async fn wait_exit(child: &mut tokio::process::Child, log: &Path) -> std::process::ExitStatus {
    match tokio::time::timeout(Duration::from_secs(8), child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!(
                "Fixture Agent did not exit within 8 seconds\n{}",
                diagnostics(log)
            );
        }
    }
}

fn command(
    binary: &Path,
    config: &Path,
    root: &Path,
    recover: bool,
    log: &Path,
) -> tokio::process::Command {
    let output = fs::File::create(log).unwrap();
    let mut command = tokio::process::Command::new(binary);
    command
        .env_clear()
        .env("PATH", root.join("empty"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .args([
            "--server",
            "http://127.0.0.1:9",
            "--config",
            config.to_str().unwrap(),
            "--no-browser",
        ])
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .kill_on_drop(true);
    if recover {
        command.arg("--recover-lock");
    }
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_marker_blocks_updates_until_profile_is_recovered() {
    let fixture = Fixture(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-installed-lifecycle-{}",
        random::hex_token::<8>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    let prefix = fixture.0.join("install");
    fs::create_dir(&prefix).unwrap();
    fs::set_permissions(&prefix, fs::Permissions::from_mode(0o700)).unwrap();
    let version = prefix.join(format!("versions/{}", env!("CARGO_PKG_VERSION")));
    fs::create_dir_all(&version).unwrap();
    fs::set_permissions(prefix.join("versions"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&version, fs::Permissions::from_mode(0o700)).unwrap();
    let installed = version.join("rdh");
    fs::copy(initial_binary::path(), &installed).unwrap();
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(
        format!("versions/{}", env!("CARGO_PKG_VERSION")),
        prefix.join("current"),
    )
    .unwrap();
    storage::write_private_new(
        &prefix.join(".native-install.json"),
        br#"{"schema":1,"kind":"native-agent"}"#,
    )
    .unwrap();
    let work = fixture.0.join("work");
    fs::create_dir(&work).unwrap();
    fs::create_dir(fixture.0.join("empty")).unwrap();
    fs::create_dir(fixture.0.join("home")).unwrap();
    let config = fixture.0.join("device.json");
    storage::write_private_new(
        &config,
        &serde_json::to_vec(&json!({"gateway_url":"http://127.0.0.1:9",
        "device_id":"fixture", "token":"x".repeat(32), "roots":{"work":work}}))
        .unwrap(),
    )
    .unwrap();
    let executable = prefix.join("current/rdh");
    let running = prefix.join(".running");
    let first_log = fixture.0.join("initial.log");
    let mut first = command(&executable, &config, &fixture.0, false, &first_log)
        .spawn()
        .unwrap();
    let first_marker = wait_marker(&running, None, &mut first, &first_log).await;
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_err());
    first.kill().await.unwrap();
    first.wait().await.unwrap();
    assert!(first_marker.exists());
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_err());
    let refused_log = fixture.0.join("refused.log");
    let mut refused = command(&executable, &config, &fixture.0, false, &refused_log)
        .spawn()
        .unwrap();
    assert!(
        !wait_exit(&mut refused, &refused_log).await.success(),
        "{}",
        diagnostics(&refused_log)
    );
    let recovered_log = fixture.0.join("recovered.log");
    let mut recovered = command(&executable, &config, &fixture.0, true, &recovered_log)
        .spawn()
        .unwrap();
    let marker = wait_marker(
        &running,
        Some(&first_marker),
        &mut recovered,
        &recovered_log,
    )
    .await;
    assert!(!first_marker.exists());
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(state["pid"], recovered.id().unwrap());
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_err());
    assert_eq!(
        unsafe { libc::kill(recovered.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    let status = wait_exit(&mut recovered, &recovered_log).await;
    assert!(status.success(), "{}", diagnostics(&recovered_log));
    assert!(!marker.exists() && !PathBuf::from(format!("{}.node-lock", config.display())).exists());
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_ok());
}
