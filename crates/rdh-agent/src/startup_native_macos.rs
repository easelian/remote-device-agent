//! Experimental launchd registration. No load, bootstrap, kickstart or bootout.
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::posix::{Created, stable_executable};
use crate::config::{DeviceConfig, home_directory};
use crate::error::{Error, Result};
use crate::startup::{Binding, Mode, StartupBackend, valid_name};

const USER_LABEL: &str = "com.be-one-click.rdh";

pub(crate) struct NativeStartup {
    config: PathBuf,
    executable: PathBuf,
    home: PathBuf,
    uid: u32,
    launchctl: PathBuf,
    timeout: Duration,
}

fn validate_paths(paths: &[&Path]) -> Result<()> {
    if paths.iter().any(|path| {
        !path.is_absolute()
            || path.to_str().is_none_or(|path| {
                path.chars()
                    .any(|c| c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}'))
            })
    }) {
        return Err(Error::message(
            "Startup requires absolute Unicode paths without control characters",
        ));
    }
    Ok(())
}

/// Absence means enabled only in a complete, recognized disabled-service list.
/// Unknown or truncated output must never authorize a future startup change.
fn launchd_enabled(output: &str, name: &str) -> Result<bool> {
    let invalid = || Error::message("Unrecognized launchctl startup state");
    let mut text = output
        .trim()
        .strip_prefix("disabled services")
        .ok_or_else(invalid)?;
    text = text.trim_start().strip_prefix('=').ok_or_else(invalid)?;
    text = text.trim_start().strip_prefix('{').ok_or_else(invalid)?;
    let mut selected = None;
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix('}') {
            if !rest.trim().is_empty() {
                return Err(invalid());
            }
            return Ok(selected.unwrap_or(true));
        }
        text = text.strip_prefix('"').ok_or_else(invalid)?;
        let mut label = String::new();
        let mut escaped = false;
        let mut end = None;
        for (index, character) in text.char_indices() {
            if escaped {
                if !matches!(character, '"' | '\\') {
                    return Err(invalid());
                }
                label.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                end = Some(index + 1);
                break;
            } else if character.is_control() {
                return Err(invalid());
            } else {
                label.push(character);
            }
        }
        text = &text[end.ok_or_else(invalid)?..];
        text = text
            .trim_start()
            .strip_prefix("=>")
            .ok_or_else(invalid)?
            .trim_start();
        let end = text
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(text.len());
        let enabled = match &text[..end] {
            "false" | "enabled" => true,
            "true" | "disabled" => false,
            _ => return Err(invalid()),
        };
        if label == name && selected.replace(enabled).is_some() {
            return Err(invalid());
        }
        text = &text[end..];
        // Current lists use whitespace; tolerate punctuation between records.
        text = text.trim_start().trim_start_matches([',', ';']);
    }
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn definition(executable: &Path, config: &Path, gateway: &str) -> Result<String> {
    validate_paths(&[executable, config])?;
    let working_directory = config
        .parent()
        .ok_or_else(|| Error::message("Invalid startup config parent"))?;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{USER_LABEL}</string>\n<key>ProgramArguments</key><array>\n"
    );
    for argument in [
        executable.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--server",
        gateway,
        "--no-browser",
        "--recover-lock",
    ] {
        xml.push_str(&format!("<string>{}</string>\n", xml_text(argument)));
    }
    xml.push_str(&format!("</array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>RunAtLoad</key><true/>\n<key>Disabled</key><true/>\n</dict></plist>\n", xml_text(working_directory.to_str().unwrap())));
    Ok(xml)
}

impl NativeStartup {
    pub(crate) fn new(config: PathBuf) -> Result<Self> {
        let executable = stable_executable()?;
        let home = home_directory()?;
        validate_paths(&[&config, &executable, &home])?;
        Ok(Self {
            config,
            executable,
            home: home.canonicalize()?,
            // SAFETY: getuid takes no arguments and cannot fail.
            uid: unsafe { libc::getuid() },
            launchctl: PathBuf::from("/bin/launchctl"),
            timeout: Duration::from_secs(12),
        })
    }
    fn domain(&self, binding: &Binding) -> Result<String> {
        if !valid_name(&binding.name) {
            return Err(Error::message("Invalid startup binding"));
        }
        match binding.mode {
            Mode::User => Ok(format!("gui/{}", self.uid)),
            Mode::Service => Ok("system".into()),
            Mode::None => Err(Error::message("Invalid startup binding")),
        }
    }
    fn checked(&self, args: &[&str]) -> Result<String> {
        let (status, output) = super::posix::run(&self.launchctl, args, self.timeout)?;
        if !status.success() {
            return Err(Error::message(
                "Startup manager operation failed; inspect native state before retrying",
            ));
        }
        Ok(output)
    }
    fn registered(&self, binding: &Binding) -> Result<()> {
        self.domain(binding)?;
        let (directory, owner) = if binding.mode == Mode::User {
            (self.home.join("Library/LaunchAgents"), self.uid)
        } else {
            (PathBuf::from("/Library/LaunchDaemons"), 0)
        };
        let file = directory.join(format!("{}.plist", binding.name));
        let metadata = fs::symlink_metadata(file)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != owner
            || metadata.mode() & 0o022 != 0
        {
            return Err(Error::message(
                "Startup registration must be an owned regular file without aliases or external write access",
            ));
        }
        Ok(())
    }
    fn user_directory(&self) -> Result<PathBuf> {
        let directory = self.home.join("Library/LaunchAgents");
        for path in [&self.home, &self.home.join("Library"), &directory] {
            match fs::symlink_metadata(path) {
                Ok(_) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound && path != &self.home =>
                {
                    DirBuilder::new().mode(0o700).create(path)?;
                }
                Err(error) => return Err(error.into()),
            }
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir() || metadata.uid() != self.uid || metadata.mode() & 0o022 != 0 {
                return Err(Error::message(
                    "User startup directory must be owned by the current user and not writable by others",
                ));
            }
        }
        Ok(directory)
    }
}

impl StartupBackend for NativeStartup {
    fn enabled(&self, binding: &Binding) -> Result<bool> {
        self.registered(binding)?;
        let output = self.checked(&["print-disabled", &self.domain(binding)?])?;
        launchd_enabled(&output, &binding.name)
    }
    fn set(&self, binding: &Binding, enabled: bool) -> Result<()> {
        self.registered(binding)?;
        let target = format!("{}/{}", self.domain(binding)?, binding.name);
        self.checked(&[if enabled { "enable" } else { "disable" }, &target])?;
        if self.enabled(binding)? != enabled {
            return Err(Error::message(
                "Startup manager did not confirm the requested state; inspect native state before retrying",
            ));
        }
        Ok(())
    }
    fn create_user(&self) -> Result<Binding> {
        let config = DeviceConfig::read(&self.config)?;
        if !self.executable.is_file() {
            return Err(Error::message("A persistent Agent executable is required"));
        }
        let contents = definition(&self.executable, &self.config, &config.gateway_url)?;
        let directory = self.user_directory()?;
        let path = directory.join(format!("{USER_LABEL}.plist"));
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(Error::message(
                    "Startup registration already exists; bind it locally after inspection",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let binding = Binding {
            mode: Mode::User,
            name: USER_LABEL.into(),
        };
        let domain = self.domain(&binding)?;
        let target = format!("{domain}/{USER_LABEL}");
        // Persist and confirm the disabled override before publishing RunAtLoad.
        // A failure may have changed the override; the caller reports unknown.
        self.checked(&["disable", &target])?;
        if launchd_enabled(&self.checked(&["print-disabled", &domain])?, USER_LABEL)? {
            return Err(Error::message(
                "Startup manager did not confirm the disabled registration",
            ));
        }
        let mut created = Created::write(&path, contents.as_bytes(), 0o600)?;
        crate::storage::sync_directory(&File::open(&directory)?)?;
        created.retain = true;
        Ok(binding)
    }
    fn service_authority(&self) -> bool {
        self.uid == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use serde_json::Value;
    use serde_json::json;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[cfg(target_os = "macos")]
    use std::process::Command;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh launchd ' &$%<>\"-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let config = path.join("device ' &<.json");
            crate::storage::write_private_new(&config, &serde_json::to_vec(&json!({"gateway_url":"http://127.0.0.1:9", "device_id":"fixture", "token":"FIXTURE_SECRET".repeat(3), "roots":{"work":path}})).unwrap()).unwrap();
            fs::write(
                path.join("rdh native ' &$%<>\""),
                "#!/bin/sh\npwd -P\nprintf '%s\\n' \"$@\"\n",
            )
            .unwrap();
            fs::set_permissions(
                path.join("rdh native ' &$%<>\""),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::write(path.join("launchctl"), format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\ndisable) printf true > \"$0.state\";;\nenable) printf false > \"$0.state\";;\nprint-disabled) value=$(cat \"$0.state\"); printf 'disabled services = {{ \"{USER_LABEL}\" => %s }}\\n' \"$value\";;\n*) exit 23;;\nesac\n")).unwrap();
            fs::set_permissions(path.join("launchctl"), fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn backend(&self) -> NativeStartup {
            NativeStartup {
                config: self.0.join("device ' &<.json"),
                executable: self.0.join("rdh native ' &$%<>\""),
                home: self.0.clone(),
                uid: unsafe { libc::getuid() },
                launchctl: self.0.join("launchctl"),
                timeout: Duration::from_secs(2),
            }
        }
        fn plist(&self) -> PathBuf {
            self.0
                .join(format!("Library/LaunchAgents/{USER_LABEL}.plist"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_launchctl_status_format_is_recognized_without_changes() {
        let (status, output) = super::super::posix::run(
            Path::new("/bin/launchctl"),
            &["print-disabled", "system"],
            Duration::from_secs(12),
        )
        .unwrap();
        assert!(status.success(), "Native launchctl status query failed");
        launchd_enabled(&output, USER_LABEL).unwrap();
    }

    #[test]
    fn disabled_overrides_match_legacy_values_and_reject_ambiguous_output() {
        for value in ["false", "enabled"] {
            assert!(
                launchd_enabled(
                    &format!("disabled services = {{ \"{USER_LABEL}\" => {value} }}"),
                    USER_LABEL
                )
                .unwrap()
            );
        }
        for value in ["true", "disabled"] {
            assert!(
                !launchd_enabled(
                    &format!("disabled services = {{ \"{USER_LABEL}\" => {value} }}"),
                    USER_LABEL
                )
                .unwrap()
            );
        }
        assert!(launchd_enabled("disabled services = { \"other\" => true }", USER_LABEL).unwrap());
        assert!(launchd_enabled("disabled services = { }", USER_LABEL).unwrap());
        for value in [
            "",
            "garbage",
            "disabled services = {",
            "disabled services = { \"other\" => invalid }",
            "disabled services = { } trailing",
        ] {
            assert!(launchd_enabled(value, USER_LABEL).is_err(), "{value}");
        }
        assert!(
            launchd_enabled(
                &format!(
                    "disabled services = {{ \"{USER_LABEL}\" => false \"{USER_LABEL}\" => true }}"
                ),
                USER_LABEL
            )
            .is_err()
        );
    }

    #[test]
    fn registration_is_disabled_before_publication_and_only_changes_future_startup() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let binding = backend.create_user().unwrap();
        let calls = fs::read_to_string(fixture.0.join("launchctl.calls")).unwrap();
        assert_eq!(
            calls,
            format!(
                "disable gui/{}/{USER_LABEL}\nprint-disabled gui/{}\n",
                backend.uid, backend.uid
            )
        );
        assert!(!backend.enabled(&binding).unwrap());
        backend.set(&binding, true).unwrap();
        assert!(backend.enabled(&binding).unwrap());
        backend.set(&binding, false).unwrap();
        assert!(!backend.enabled(&binding).unwrap());
        let calls = fs::read_to_string(fixture.0.join("launchctl.calls")).unwrap();
        assert!(calls.lines().all(|line| matches!(
            line.split_whitespace().next(),
            Some("enable" | "disable" | "print-disabled")
        )));
        assert_eq!(
            backend
                .domain(&Binding {
                    mode: Mode::Service,
                    name: "custom.daemon".into()
                })
                .unwrap(),
            "system"
        );
        assert!(
            backend
                .domain(&Binding {
                    mode: Mode::None,
                    name: USER_LABEL.into()
                })
                .is_err()
        );
        assert!(
            backend
                .domain(&Binding {
                    mode: Mode::User,
                    name: "bad/name".into()
                })
                .is_err()
        );
        assert_eq!(backend.service_authority(), backend.uid == 0);
        assert!(
            NativeStartup::new(backend.config.clone())
                .unwrap()
                .config
                .is_absolute()
        );
        let bytes = fs::read(fixture.plist()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("FIXTURE_SECRET"));
        assert_eq!(fs::metadata(fixture.plist()).unwrap().mode() & 0o777, 0o600);
        let calls_before = fs::read(fixture.0.join("launchctl.calls")).unwrap();
        assert!(backend.create_user().is_err());
        assert_eq!(fs::read(fixture.plist()).unwrap(), bytes);
        assert_eq!(
            fs::read(fixture.0.join("launchctl.calls")).unwrap(),
            calls_before
        );
    }

    #[test]
    fn unsafe_directories_and_unknown_manager_state_do_not_publish_registration() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("Library")).unwrap();
        symlink(&fixture.0, fixture.0.join("Library/LaunchAgents")).unwrap();
        assert!(fixture.backend().create_user().is_err());
        assert!(!fixture.0.join("launchctl.calls").exists());
        fs::remove_file(fixture.0.join("Library/LaunchAgents")).unwrap();
        fs::write(fixture.0.join("launchctl"), "#!/bin/sh\nprintf garbage\n").unwrap();
        assert!(fixture.backend().create_user().is_err());
        assert!(!fixture.plist().exists());
        fs::remove_file(fixture.0.join("rdh native ' &$%<>\"")).unwrap();
        assert!(fixture.backend().create_user().is_err());
        assert!(!fixture.plist().exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the native macOS plutil parser"]
    fn launchagent_plist_executes_exact_native_arguments() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        backend.create_user().unwrap();
        #[cfg(target_os = "macos")]
        assert!(
            Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(fixture.plist())
                .status()
                .unwrap()
                .success()
        );
        let parsed = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(fixture.plist())
            .output()
            .unwrap();
        assert!(
            parsed.status.success(),
            "{}",
            String::from_utf8_lossy(&parsed.stderr)
        );
        let value: Value = serde_json::from_slice(&parsed.stdout).unwrap();
        assert_eq!(value["Label"], USER_LABEL);
        assert_eq!(value["RunAtLoad"], true);
        assert_eq!(value["Disabled"], true);
        let arguments: Vec<_> = value["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argument| argument.as_str().unwrap())
            .collect();
        let output = Command::new(arguments[0])
            .args(&arguments[1..])
            .current_dir(value["WorkingDirectory"].as_str().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lines: Vec<_> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .collect();
        assert_eq!(
            json!(lines),
            json!([
                fixture.0,
                "--config",
                backend.config,
                "--server",
                "http://127.0.0.1:9",
                "--no-browser",
                "--recover-lock"
            ])
        );
        assert!(
            !value
                .as_object()
                .unwrap()
                .contains_key("EnvironmentVariables")
        );
    }
}
