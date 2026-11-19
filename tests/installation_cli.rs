#![cfg(target_os = "linux")]

use rdh_agent::config::{DEFAULT_SERVER, config_paths};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rdh-installation-cli-{}",
            rdh_agent::random::hex_token::<8>().unwrap()
        ));
        rdh_agent::storage::create_private_dir(&path).unwrap();
        Self(path)
    }

    fn command(&self, executable: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(executable);
        command
            .args(args)
            .env("XDG_CONFIG_HOME", &self.0)
            .env("XDG_RUNTIME_DIR", &self.0)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/absent-bus", self.0.display()),
            )
            .env_remove("RDH_NPM_ENTRY");
        command
    }

    fn run(&self, executable: &Path, args: &[&str]) -> Output {
        self.command(executable, args).output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn private_file(path: &Path, data: &str, mode: u32) {
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn doctor_and_npm_update_keep_binding_and_startup_files_untouched() {
    let fixture = Fixture::new();
    let native = fixture.0.join("rdh");
    symlink(env!("CARGO_BIN_EXE_rdh"), &native).unwrap();
    let config = config_paths(&fixture.0, DEFAULT_SERVER, "default").unwrap()[0].clone();
    rdh_agent::storage::create_private_dir(config.parent().unwrap()).unwrap();
    let registration = PathBuf::from(format!("{}.startup.json", config.display()));
    let wrapper = PathBuf::from(format!("{}.startup-run", config.display()));
    let pending_unbind = PathBuf::from(format!("{}.unbind.json", config.display()));
    let unit_path = fixture.0.join("systemd/user/rdh.service");
    fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
    let startup_path = fixture.0.join("persistent Agent's bin/rdh");
    let script = format!(
        "#!/bin/sh\nexec '{}'\n",
        startup_path.display().to_string().replace('\'', "'\\''")
    );
    let unit = format!("[Service]\nExecStart=\"{}\"\n", wrapper.display());
    let documents = [
        (
            &config,
            "This file must not be parsed by diagnostics",
            0o600,
        ),
        (
            &registration,
            r#"{"version":1,"binding":{"mode":"user","name":"rdh.service"},"pending":null}"#,
            0o600,
        ),
        (&wrapper, script.as_str(), 0o700),
        (&unit_path, unit.as_str(), 0o600),
        (&pending_unbind, r#"{"version":1,"action":"unbind"}"#, 0o600),
    ];
    for (path, data, mode) in documents {
        private_file(path, data, mode);
    }

    let result = fixture.run(&native, &["doctor"]);
    assert!(result.status.success(), "{:?}", result);
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(output.contains(&format!("Command entry: {}", native.display())));
    assert!(output.contains("Installation source: native"));
    assert!(output.contains(&format!("Startup executable: {}", startup_path.display())));
    let native_update = fixture.run(&native, &["update"]);
    assert!(!native_update.status.success());
    assert!(String::from_utf8_lossy(&native_update.stderr).contains("pending unbind"));

    let package = fixture
        .0
        .join("node_modules/@easelian/remote-device-agent-linux-x64-gnu");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("package.json"), "{}").unwrap();
    let npm_binary = package.join("rdh");
    fs::copy(env!("CARGO_BIN_EXE_rdh"), &npm_binary).unwrap();
    let npm_update = fixture.run(&npm_binary, &["update"]);
    assert!(!npm_update.status.success());
    assert!(
        String::from_utf8_lossy(&npm_update.stderr)
            .contains("npm install -g @easelian/remote-device-agent")
    );
    let npm_doctor = fixture.run(&npm_binary, &["doctor"]);
    assert!(npm_doctor.status.success());
    assert!(String::from_utf8_lossy(&npm_doctor.stdout).contains("Installation source: npm"));

    for (path, data, mode) in documents {
        assert_eq!(
            fs::read(path).unwrap(),
            data.as_bytes(),
            "{}",
            path.display()
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
}

#[test]
fn doctor_lists_shadowed_and_off_path_installations_once() {
    let fixture = Fixture::new();
    let native_dir = fixture.0.join("native/bin");
    let native = native_dir.join("rdh");
    let alias_dir = fixture.0.join("alias");
    let alias = alias_dir.join("rdh");
    let npm_bin = fixture.0.join("npm/bin");
    let npm = npm_bin.join("rdh");
    let npm_package = fixture
        .0
        .join("npm/lib/node_modules/@easelian/remote-device-agent");
    let launcher = npm_package.join("rdh.cjs");
    let custom_dir = fixture.0.join("off-path");
    let custom = custom_dir.join("rdh");
    for dir in [&native_dir, &alias_dir, &npm_bin, &npm_package, &custom_dir] {
        fs::create_dir_all(dir).unwrap();
    }
    symlink(env!("CARGO_BIN_EXE_rdh"), &native).unwrap();
    symlink(&native, &alias).unwrap();
    fs::write(npm_package.join("package.json"), "{}").unwrap();
    let marker = fixture.0.join("ran-another-installation");
    private_file(
        &launcher,
        &format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
        0o755,
    );
    symlink(&launcher, &npm).unwrap();
    private_file(&custom, "#!/bin/sh\nexit 7\n", 0o755);

    let result = fixture
        .command(&native, &["doctor"])
        .env(
            "PATH",
            std::env::join_paths([&native_dir, &alias_dir, &npm_bin, &npm_bin]).unwrap(),
        )
        .env("RDH_INSTALL_DIR", &custom_dir)
        .output()
        .unwrap();
    assert!(result.status.success(), "{:?}", result);
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(output.contains(&format!("Command entry: {}", native.display())));
    assert!(output.contains("Other rdh installations:"));
    assert_eq!(
        output
            .matches(&format!("  {} (npm)", npm.display()))
            .count(),
        1
    );
    assert!(output.contains(&format!("  {} (native or custom)", custom.display())));
    assert!(!output.contains(&alias.display().to_string()));
    assert!(!marker.exists());
}
