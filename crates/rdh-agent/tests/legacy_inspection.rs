#![cfg(target_os = "linux")]

#[path = "support/initial_binary.rs"]
mod initial_binary;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rdh_agent::{random, storage};
use serde_json::{Value, json};

struct Fixture {
    root: PathBuf,
    prefix: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rdh-legacy-inspect-{}",
            random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let prefix = root.join("installation with spaces");
        let version = prefix.join("versions/0.7.2");
        fs::create_dir_all(version.join("bin")).unwrap();
        fs::create_dir(version.join("bundle")).unwrap();
        for directory in [
            prefix.clone(),
            prefix.join("versions"),
            version.clone(),
            version.join("bin"),
            version.join("bundle"),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::copy(initial_binary::path(), version.join("node")).unwrap();
        fs::set_permissions(version.join("node"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(version.join("rdh"), b"#!/bin/sh\nexit 9\n").unwrap();
        fs::set_permissions(version.join("rdh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(version.join("bin/remote-device-hub.mjs"), b"entry\n").unwrap();
        fs::write(version.join("bundle/agent.mjs"), b"bundle\n").unwrap();
        fs::write(
            version.join("package.json"),
            br#"{"name":"remote-device-hub","version":"0.7.2"}"#,
        )
        .unwrap();
        for file in [
            version.join("bin/remote-device-hub.mjs"),
            version.join("bundle/agent.mjs"),
            version.join("package.json"),
        ] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        symlink("versions/0.7.2", prefix.join("current")).unwrap();
        let config = root.join("device.json");
        storage::write_private_new(
            &config,
            &serde_json::to_vec(&json!({"gateway_url":"http://127.0.0.1:9",
                "device_id":"fixture", "token":"fixture-token-must-not-be-printed-123456",
                "roots":{"work":root}}))
            .unwrap(),
        )
        .unwrap();
        Self {
            root,
            prefix,
            config,
        }
    }

    fn inspect(&self, prefix: &Path, config: &Path, extra: &[&str]) -> Output {
        Command::new(initial_binary::path())
            .env_clear()
            .env("PATH", self.root.join("no-runtime"))
            .args(["migrate", "--inspect", "--prefix"])
            .arg(prefix)
            .arg("--config")
            .arg(config)
            .args(extra)
            .output()
            .unwrap()
    }

    fn run(&self) -> Output {
        self.inspect(&self.prefix, &self.config, &[])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn inventory_is_read_only_and_reports_only_observed_properties() {
    let fixture = Fixture::new();
    let config_before = fs::read(&fixture.config).unwrap();
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output_text = String::from_utf8(output.stdout).unwrap();
    assert!(!output_text.contains("fixture-token-must-not-be-printed"));
    let result: Value = serde_json::from_str(&output_text).unwrap();
    assert_eq!(result["kind"], "legacy-node-layout");
    assert_eq!(result["version"], "0.7.2");
    assert_eq!(result["device_id"], "fixture");
    assert_eq!(result["config_parse"], "ok");
    assert_eq!(result["provenance"], "unverified");
    assert_eq!(result["hub_ready"], "unverified");
    assert_eq!(result["active_work"], "unverified");
    assert_eq!(result["migration_authorized"], false);
    assert_eq!(
        result["offline_migration"],
        "requires_explicit_stopped_service_preflight"
    );
    assert!(result["service"].is_null());
    assert_eq!(result["startup"]["state"], "missing");
    assert!(result["startup"]["binding"].is_null());
    assert!(result["startup"]["binding_matches_user_unit"].is_null());
    assert_eq!(result["startup"]["state_lock_present"], false);
    for name in ["node", "entry", "bundle", "launcher", "package"] {
        assert_eq!(result["sha256"][name].as_str().unwrap().len(), 64);
    }
    assert_eq!(fs::read(&fixture.config).unwrap(), config_before);
    assert_eq!(
        fs::read_link(fixture.prefix.join("current")).unwrap(),
        Path::new("versions/0.7.2")
    );
    assert!(!fixture.prefix.join("install-id").exists());
    assert!(!fixture.prefix.join(".native-install.json").exists());
    assert!(!fixture.root.join("device.json.node-lock").exists());
}

#[test]
fn inventory_reports_existing_startup_binding_without_exposing_or_changing_approval() {
    let fixture = Fixture::new();
    let sidecar = fixture.root.join("device.json.startup.json");
    let request_id = "01234567-89ab-cdef-0123-456789abcdef";
    storage::write_private_new(
        &sidecar,
        &serde_json::to_vec(&json!({
            "version": 1,
            "binding": {"mode":"user", "name":"rdh.service"},
            "pending": {"request_id":request_id, "mode":"user", "expires_at": 9_007_199_254_740_991u64},
        }))
        .unwrap(),
    )
    .unwrap();
    let before = fs::read(&sidecar).unwrap();
    let lock = fixture.root.join("device.json.startup.json.node-lock");
    let guard = fixture
        .root
        .join("device.json.startup.json.node-lock.reclaiming");
    fs::create_dir(&lock).unwrap();
    fs::create_dir(&guard).unwrap();
    let output = fixture.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains(request_id));
    let value: Value = serde_json::from_str(&text).unwrap();
    let startup = &value["startup"];
    assert_eq!(startup["state"], "readable");
    assert_eq!(
        startup["binding"],
        json!({"mode":"user", "name":"rdh.service"})
    );
    assert!(startup["binding_matches_user_unit"].is_null());
    assert_eq!(startup["pending_mode"], "user");
    assert_eq!(startup["state_lock_present"], true);
    assert_eq!(startup["recovery_guard_present"], true);
    assert_eq!(fs::read(&sidecar).unwrap(), before);
    assert!(lock.is_dir() && guard.is_dir());

    fs::write(&sidecar, b"{invalid").unwrap();
    let output = fixture.run();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["startup"]["state"], "unavailable");
    assert!(value["startup"]["binding"].is_null());
    assert!(value["startup"]["pending_mode"].is_null());
    fs::remove_file(&sidecar).unwrap();
    symlink(&fixture.config, &sidecar).unwrap();
    let output = fixture.run();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["startup"]["state"], "unavailable");
}

#[test]
fn inventory_rejects_redirected_or_ambiguous_legacy_layouts() {
    let fixture = Fixture::new();
    let version = fixture.prefix.join("versions/0.7.2");
    let link = fixture.root.join("prefix-link");
    symlink(&fixture.prefix, &link).unwrap();
    assert!(
        !fixture
            .inspect(&link, &fixture.config, &[])
            .status
            .success()
    );
    let config_link = fixture.root.join("config-link");
    symlink(&fixture.config, &config_link).unwrap();
    assert!(
        !fixture
            .inspect(&fixture.prefix, &config_link, &[])
            .status
            .success()
    );
    fs::write(fixture.prefix.join(".native-install.json"), "{}").unwrap();
    assert!(!fixture.run().status.success());
    fs::remove_file(fixture.prefix.join(".native-install.json")).unwrap();
    fs::write(
        version.join("package.json"),
        br#"{"name":"remote-device-hub","version":"0.7.1"}"#,
    )
    .unwrap();
    assert!(!fixture.run().status.success());
    fs::write(
        version.join("package.json"),
        br#"{"name":"remote-device-hub","version":"0.7.2"}"#,
    )
    .unwrap();
    let alias = version.join("node-alias");
    fs::hard_link(version.join("node"), &alias).unwrap();
    assert!(!fixture.run().status.success());
    fs::remove_file(alias).unwrap();
    fs::set_permissions(&version, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(!fixture.run().status.success());
    fs::set_permissions(&version, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(fixture.prefix.join("current")).unwrap();
    symlink("versions/../0.7.2", fixture.prefix.join("current")).unwrap();
    assert!(!fixture.run().status.success());
}
