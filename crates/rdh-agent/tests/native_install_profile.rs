#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]

#[path = "support/initial_binary.rs"]
mod initial_binary;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rdh_agent::{profile_lock::ProfileLocks, random, storage};
use serde_json::json;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(initial_binary::path())
        .env_clear()
        .env("PATH", root.join("empty"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn prepare_existing_profile_requires_lock_and_valid_private_inputs() {
    let fixture = Fixture(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-prepare-profile-{}",
        random::hex_token::<8>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["empty", "home", "work"] {
        fs::create_dir(fixture.0.join(name)).unwrap();
    }
    let config = fixture.0.join("old device.json");
    let token = "fixture-token-must-not-be-copied-or-printed";
    storage::write_private_new(
        &config,
        &serde_json::to_vec(&json!({"gateway_url":"http://127.0.0.1:9",
            "device_id":"fixture", "token":token, "roots":{"work":fixture.0.join("work")}}))
        .unwrap(),
    )
    .unwrap();
    let original = fs::read(&config).unwrap();
    let prefix = fixture.0.join("new native install");
    let args = [
        "install-native",
        "--prefix",
        prefix.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ];

    let lock = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    let lock_path = PathBuf::from(format!("{}.rdh-lock", config.display()));
    let owner = fs::read(lock_path.join("owner.json")).unwrap();
    let busy = run(&fixture.0, &args);
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("locked"));
    assert!(!prefix.exists());
    assert_eq!(fs::read(lock_path.join("owner.json")).unwrap(), owner);
    assert_eq!(fs::read(&config).unwrap(), original);
    drop(lock);

    let alias = fixture.0.join("hardlink.json");
    fs::hard_link(&config, &alias).unwrap();
    assert!(!run(&fixture.0, &args).status.success());
    assert!(!prefix.exists());
    fs::remove_file(alias).unwrap();
    let link = fixture.0.join("config-link.json");
    symlink(&config, &link).unwrap();
    assert!(
        !run(
            &fixture.0,
            &[
                "install-native",
                "--prefix",
                prefix.to_str().unwrap(),
                "--config",
                link.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(!prefix.exists());

    let policy = config.with_extension("policy.json");
    storage::write_private_new(&policy, br#"{"version":999}"#).unwrap();
    assert!(!run(&fixture.0, &args).status.success());
    assert!(!prefix.exists());
    assert!(!lock_path.exists());
    fs::remove_file(policy).unwrap();

    let installed = run(&fixture.0, &args);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(!String::from_utf8_lossy(&installed.stdout).contains(token));
    assert!(!String::from_utf8_lossy(&installed.stderr).contains(token));
    assert_eq!(fs::read(&config).unwrap(), original);
    assert!(!lock_path.exists());
    let entries: Vec<_> = fs::read_dir(prefix.join(".profiles"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    let record = fs::read_to_string(&entries[0]).unwrap();
    assert!(!record.contains(token));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&record).unwrap()["config"],
        config.to_str().unwrap()
    );
    assert_eq!(
        fs::metadata(&entries[0]).unwrap().permissions().mode() & 0o077,
        0
    );
}
