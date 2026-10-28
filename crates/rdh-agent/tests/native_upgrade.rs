#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

use rdh_agent::{config::sha256_hex, profile_lock::ProfileLocks, random, storage};
use serde_json::json;
use tokio::process::Command;

#[path = "support/native_candidate.rs"]
mod native_candidate;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", root.join("empty"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"));
    command
}
async fn run(binary: &Path, root: &Path, args: &[&str]) -> std::process::Output {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    eprintln!("Running fixture command: {args:?}");
    let started = Instant::now();
    let mut child = command(binary, root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.take(65536).read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let err = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(65536).read_to_end(&mut bytes).await.unwrap();
        bytes
    });
    let status = match tokio::time::timeout(Duration::from_secs(20), child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            // Retain ownership across the wait timeout: explicitly terminate
            // and reap only this fixture child before its directory is removed.
            let _ = child.kill().await;
            let _ = child.wait().await;
            let stdout = out.await.unwrap();
            let stderr = err.await.unwrap();
            panic!(
                "Fixture command timed out after {:?}: {args:?}\nstdout: {}\nstderr: {}",
                started.elapsed(),
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
            );
        }
    };
    let stdout = out.await.unwrap();
    let stderr = err.await.unwrap();
    assert!(
        stdout.len() < 65536 && stderr.len() < 65536,
        "Fixture output was truncated: {args:?}"
    );
    eprintln!(
        "Fixture command completed in {:?}: {args:?}",
        started.elapsed()
    );
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_install_offline_upgrade_rollback_and_interrupted_recovery() {
    let (next, bytes) = native_candidate::load();
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "rdh-native-upgrade-{}",
        random::hex_token::<8>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(fixture.0.join("empty")).unwrap();
    fs::create_dir(fixture.0.join("home")).unwrap();
    let work = fixture.0.join("work");
    fs::create_dir(&work).unwrap();
    let config = fixture.0.join("device.json");
    storage::write_private_new(
        &config,
        &serde_json::to_vec(&json!({"gateway_url":"http://127.0.0.1:9",
        "device_id":"fixture", "token":"x".repeat(32), "roots":{"work":work}}))
        .unwrap(),
    )
    .unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_rdh"));
    let prefix = fixture.0.join("install");
    let installed = run(
        binary,
        &fixture.0,
        &[
            "install-native",
            "--prefix",
            prefix.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
    )
    .await;
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let current = prefix.join("current/rdh");
    let registered = run(&current, &fixture.0, &["upgrade", "--list-profiles"]).await;
    assert!(registered.status.success());
    assert_eq!(
        String::from_utf8(registered.stdout).unwrap().trim(),
        config.to_str().unwrap()
    );
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        String::from_utf8(run(&current, &fixture.0, &["--version"]).await.stdout)
            .unwrap()
            .trim(),
        version
    );
    let rejected = run(
        binary,
        &fixture.0,
        &["install-native", "--prefix", prefix.to_str().unwrap()],
    )
    .await;
    assert!(!rejected.status.success());
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );

    let candidate = fixture.0.join("candidate");
    fs::write(&candidate, &bytes).unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        String::from_utf8(run(&candidate, &fixture.0, &["--version"]).await.stdout)
            .unwrap()
            .trim(),
        next
    );
    let index = fixture.0.join("channel.json");
    let platform = match (std::env::consts::ARCH, cfg!(target_env = "musl")) {
        ("x86_64", false) => "linux-x64-gnu",
        ("x86_64", true) => "linux-x64-musl",
        ("aarch64", false) => "linux-arm64-gnu",
        ("aarch64", true) => "linux-arm64-musl",
        _ => panic!("unsupported test architecture"),
    };
    let mut channel = json!({"schema":2,"channel":"stable","version":next,
        "platforms":{(platform):{"format":"native-binary","url":"candidate", "sha256":sha256_hex(&bytes),"bytes":bytes.len()}}});
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();
    let args = [
        "upgrade",
        "--index",
        index.to_str().unwrap(),
        "--no-restart",
    ];
    let check = run(
        &current,
        &fixture.0,
        &["upgrade", "--index", index.to_str().unwrap(), "--check"],
    )
    .await;
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(!prefix.join("previous").exists());
    channel["platforms"][platform]["sha256"] = json!("0".repeat(64));
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();
    assert!(!run(&current, &fixture.0, &args).await.status.success());
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    assert!(!prefix.join(".update-pending.json").exists());
    let invalid = b"not an ELF executable";
    fs::write(&candidate, invalid).unwrap();
    channel["platforms"][platform]["sha256"] = json!(sha256_hex(invalid));
    channel["platforms"][platform]["bytes"] = json!(invalid.len());
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();
    assert!(!run(&current, &fixture.0, &args).await.status.success());
    assert!(!prefix.join("versions").join(&next).exists());
    fs::write(&candidate, &bytes).unwrap();
    channel["platforms"][platform]["bytes"] = json!(bytes.len());
    channel["platforms"][platform]["sha256"] = json!(sha256_hex(&bytes));
    fs::write(&index, serde_json::to_vec(&channel).unwrap()).unwrap();

    // A profile registered during installation protects the first upgrade,
    // even before any Agent has launched with that profile.
    let before_launch = fs::read(&config).unwrap();
    fs::write(&config, b"{broken").unwrap();
    assert!(!run(&current, &fixture.0, &args).await.status.success());
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    let staged = prefix.join("versions").join(&next).join("rdh");
    assert_eq!(sha256_hex(&fs::read(&staged).unwrap()), sha256_hex(&bytes));
    fs::write(&config, before_launch).unwrap();
    let paired = run(
        &current,
        &fixture.0,
        &[
            "--server",
            "http://127.0.0.1:9",
            "--config",
            config.to_str().unwrap(),
            "--pair-only",
        ],
    )
    .await;
    assert!(
        paired.status.success(),
        "{}",
        String::from_utf8_lossy(&paired.stderr)
    );
    let profiles: Vec<_> = fs::read_dir(prefix.join(".profiles"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(profiles.len(), 1);
    let record = fs::read_to_string(&profiles[0]).unwrap();
    assert!(record.contains(config.to_str().unwrap()));
    assert!(!record.contains(&"x".repeat(32)));
    let mut agent = command(&current, &fixture.0)
        .args([
            "--server",
            "http://127.0.0.1:9",
            "--config",
            config.to_str().unwrap(),
            "--no-browser",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if fs::read_dir(prefix.join(".running"))
                .ok()
                .and_then(|mut entries| entries.next())
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let busy = run(&current, &fixture.0, &args).await;
    assert!(!busy.status.success());
    assert_eq!(sha256_hex(&fs::read(&staged).unwrap()), sha256_hex(&bytes));
    assert_eq!(
        unsafe { libc::kill(agent.id().unwrap() as i32, libc::SIGTERM) },
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(8), agent.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );

    let original_config = fs::read(&config).unwrap();
    fs::write(&config, b"{broken").unwrap();
    let malformed = run(&current, &fixture.0, &args).await;
    assert!(!malformed.status.success());
    assert!(!String::from_utf8_lossy(&malformed.stderr).contains(&"x".repeat(32)));
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    assert!(!prefix.join(".update-pending.json").exists());
    fs::remove_file(&config).unwrap();
    assert!(!run(&current, &fixture.0, &args).await.status.success());
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    storage::write_private_new(&config, &original_config).unwrap();
    let policy = config.with_extension("policy.json");
    storage::write_private_new(&policy, br#"{"version":999}"#).unwrap();
    let bad_policy = run(&current, &fixture.0, &args).await;
    assert!(!bad_policy.status.success());
    assert!(!String::from_utf8_lossy(&bad_policy.stderr).contains(&"x".repeat(32)));
    fs::remove_file(&policy).unwrap();
    let locked = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    assert!(!run(&current, &fixture.0, &args).await.status.success());
    drop(locked);
    let upgrade = run(&current, &fixture.0, &args).await;
    assert!(
        upgrade.status.success(),
        "{}",
        String::from_utf8_lossy(&upgrade.stderr)
    );
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{next}"))
    );
    assert_eq!(
        fs::read_link(prefix.join("previous")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    let rollback = run(&current, &fixture.0, &["rollback", "--no-restart"]).await;
    assert!(
        rollback.status.success(),
        "{}",
        String::from_utf8_lossy(&rollback.stderr)
    );
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    assert_eq!(
        fs::read_link(prefix.join("previous")).unwrap(),
        PathBuf::from(format!("versions/{next}"))
    );

    let mut crashed = command(&current, &fixture.0)
        .args([
            "--server",
            "http://127.0.0.1:9",
            "--config",
            config.to_str().unwrap(),
            "--no-browser",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if fs::read_dir(prefix.join(".running"))
                .ok()
                .and_then(|mut entries| entries.next())
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    crashed.kill().await.unwrap();
    crashed.wait().await.unwrap();
    storage::write_private_new(
        &prefix.join(".update-pending.json"),
        &serde_json::to_vec(&json!({"schema":1,
        "old_current":version, "old_previous":next, "new_current":next}))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(prefix.join("previous")).unwrap();
    symlink(format!("versions/{version}"), prefix.join("previous")).unwrap();
    fs::remove_file(prefix.join("current")).unwrap();
    symlink(format!("versions/{next}"), prefix.join("current")).unwrap();
    let blocked = run(
        &current,
        &fixture.0,
        &[
            "--server",
            "http://127.0.0.1:9",
            "--config",
            config.to_str().unwrap(),
            "--pair-only",
        ],
    )
    .await;
    assert!(!blocked.status.success());
    assert!(
        !run(&current, &fixture.0, &["upgrade", "--recover"])
            .await
            .status
            .success()
    );
    let recovered = run(
        &current,
        &fixture.0,
        &["upgrade", "--recover", "--recover-lock"],
    )
    .await;
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_link(prefix.join("current")).unwrap(),
        PathBuf::from(format!("versions/{version}"))
    );
    assert_eq!(
        fs::read_link(prefix.join("previous")).unwrap(),
        PathBuf::from(format!("versions/{next}"))
    );
    assert!(!prefix.join(".update-pending.json").exists());
    assert!(!PathBuf::from(format!("{}.node-lock", config.display())).exists());
    // An updater interrupted before the pointer transaction can leave a
    // legacy-compatible profile lock even when no running marker exists.
    let mut abandoned = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    let lock_path = PathBuf::from(format!("{}.node-lock", config.display()));
    let owner_path = lock_path.join("owner.json");
    let mut owner: serde_json::Value =
        serde_json::from_slice(&fs::read(&owner_path).unwrap()).unwrap();
    owner["pid"] = json!(i32::MAX);
    fs::write(&owner_path, serde_json::to_vec(&owner).unwrap()).unwrap();
    abandoned.retain_on_drop();
    drop(abandoned);
    let profile_recovery = run(
        &current,
        &fixture.0,
        &[
            "--server",
            "http://127.0.0.1:9",
            "--config",
            config.to_str().unwrap(),
            "--recover-lock",
            "--pair-only",
        ],
    )
    .await;
    assert!(
        profile_recovery.status.success(),
        "{}",
        String::from_utf8_lossy(&profile_recovery.stderr)
    );
    assert!(!lock_path.exists());
    let listed = run(&current, &fixture.0, &["upgrade", "--list-profiles"]).await;
    assert!(listed.status.success());
    assert_eq!(
        String::from_utf8(listed.stdout).unwrap().trim(),
        config.to_str().unwrap()
    );
    let locked = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    assert!(
        !run(
            &current,
            &fixture.0,
            &["upgrade", "--forget-profile", config.to_str().unwrap()]
        )
        .await
        .status
        .success()
    );
    drop(locked);
    assert!(
        run(
            &current,
            &fixture.0,
            &["upgrade", "--forget-profile", config.to_str().unwrap()]
        )
        .await
        .status
        .success()
    );
    assert!(config.is_file());
    assert_eq!(fs::read_dir(prefix.join(".profiles")).unwrap().count(), 0);
}
