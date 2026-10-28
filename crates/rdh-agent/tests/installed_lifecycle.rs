#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::Duration;
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

async fn wait_marker(directory: &Path) -> PathBuf {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(entries) = fs::read_dir(directory) {
                if let Some(entry) = entries.filter_map(|entry| entry.ok()).next() {
                    return entry.path();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native running marker was not created")
}

fn command(binary: &Path, config: &Path, root: &Path, recover: bool) -> tokio::process::Command {
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if recover {
        command.arg("--recover-lock");
    }
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_marker_blocks_updates_until_profile_is_recovered() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
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
    fs::copy(env!("CARGO_BIN_EXE_rdh"), &installed).unwrap();
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
    let mut first = command(&executable, &config, &fixture.0, false)
        .spawn()
        .unwrap();
    let first_marker = wait_marker(&running).await;
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_err());
    first.kill().await.unwrap();
    first.wait().await.unwrap();
    assert!(first_marker.exists());
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_err());
    let refused = command(&executable, &config, &fixture.0, false)
        .status()
        .await
        .unwrap();
    assert!(!refused.success());
    let mut recovered = command(&executable, &config, &fixture.0, true)
        .spawn()
        .unwrap();
    let marker = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let marker = wait_marker(&running).await;
            if marker != first_marker {
                break marker;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!first_marker.exists());
    let state: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(state["pid"], recovered.id().unwrap());
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_err());
    assert_eq!(
        unsafe { libc::kill(recovered.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    let status = tokio::time::timeout(Duration::from_secs(8), recovered.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!marker.exists() && !PathBuf::from(format!("{}.node-lock", config.display())).exists());
    assert!(ManagerLock::acquire(&prefix, &installed, false).is_ok());
}
