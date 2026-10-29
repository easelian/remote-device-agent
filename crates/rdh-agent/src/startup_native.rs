//! Platform registration backend. Enabling changes future startup only.

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup")
))]
#[path = "startup_posix.rs"]
pub(crate) mod posix;

#[cfg(any(
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(test, target_os = "linux")
))]
#[path = "startup_native_macos.rs"]
mod macos;

#[cfg(all(target_os = "macos", feature = "experimental-macos-startup"))]
pub(crate) use macos::NativeStartup;

#[cfg(target_os = "macos")]
pub(crate) const DEFAULT_SERVICE_NAME: &str = "com.be-one-click.rdh";
#[cfg(windows)]
pub(crate) const DEFAULT_SERVICE_NAME: &str = "RemoteDeviceHub";
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) const DEFAULT_SERVICE_NAME: &str = "rdh.service";

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, DirBuilder, File};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::time::Duration;

    use super::posix::{Created, stable_executable};
    use crate::config::{DeviceConfig, config_base};
    use crate::error::{Error, Result};
    use crate::startup::{Binding, Mode, StartupBackend};

    pub(crate) struct NativeStartup {
        config: PathBuf,
        executable: PathBuf,
        config_base: PathBuf,
        systemctl: PathBuf,
        timeout: Duration,
    }

    impl NativeStartup {
        pub(crate) fn new(config: PathBuf) -> Result<Self> {
            let executable = stable_executable()?;
            let base = config_base()?;
            if !config.is_absolute()
                || !executable.is_absolute()
                || !base.is_absolute()
                || [config.as_path(), executable.as_path(), base.as_path()]
                    .iter()
                    .any(|path| {
                        path.to_str().is_none_or(|value| {
                            value.chars().any(|character| character.is_control())
                        })
                    })
            {
                return Err(Error::message(
                    "Startup requires absolute Unicode paths without control characters",
                ));
            }
            Ok(Self {
                config,
                executable,
                config_base: base,
                systemctl: PathBuf::from("/usr/bin/systemctl"),
                timeout: Duration::from_secs(12),
            })
        }
        fn run(&self, args: &[&str]) -> Result<(ExitStatus, String)> {
            super::posix::run(&self.systemctl, args, self.timeout)
        }
        fn checked(&self, args: &[&str]) -> Result<()> {
            let (status, _) = self.run(args)?;
            if !status.success() {
                return Err(Error::message("Startup manager operation failed"));
            }
            Ok(())
        }
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
    fn unit_quote(value: &str) -> String {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    }

    impl StartupBackend for NativeStartup {
        fn enabled(&self, binding: &Binding) -> Result<bool> {
            let mut args = Vec::new();
            if binding.mode == Mode::User {
                args.push("--user");
            }
            args.extend(["is-enabled", binding.name.as_str()]);
            let (status, output) = self.run(&args)?;
            if status.success() {
                Ok(output == "enabled")
            } else if status.code() == Some(1) && output == "disabled" {
                Ok(false)
            } else {
                Err(Error::message(
                    "Startup registration is missing or its state is unavailable",
                ))
            }
        }
        fn set(&self, binding: &Binding, enabled: bool) -> Result<()> {
            let mut args = vec!["--no-ask-password"];
            if binding.mode == Mode::User {
                args.push("--user");
            }
            args.extend([
                if enabled { "enable" } else { "disable" },
                binding.name.as_str(),
            ]);
            self.checked(&args)?;
            if self.enabled(binding)? != enabled {
                return Err(Error::message(
                    "Startup manager did not confirm the requested state",
                ));
            }
            Ok(())
        }
        fn create_user(&self) -> Result<Binding> {
            let config = DeviceConfig::read(&self.config)?;
            if !self.executable.is_file() {
                return Err(Error::message("A persistent Agent executable is required"));
            }
            let directory = self.config_base.join("systemd/user");
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&directory)?;
            let metadata = fs::symlink_metadata(&directory)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != (unsafe { libc::getuid() })
                || metadata.mode() & 0o022 != 0
            {
                return Err(Error::message(
                    "User startup directory must be owned by the current user and not writable by others",
                ));
            }
            let path = self
                .config
                .to_str()
                .ok_or_else(|| Error::message("Startup config path is not Unicode"))?;
            let executable = self
                .executable
                .to_str()
                .ok_or_else(|| Error::message("Startup executable path is not Unicode"))?;
            let parent = self
                .config
                .parent()
                .and_then(Path::to_str)
                .ok_or_else(|| Error::message("Invalid startup config parent"))?;
            let mut wrapper_path = self.config.as_os_str().to_os_string();
            wrapper_path.push(".startup-run");
            let wrapper_path = PathBuf::from(wrapper_path);
            let wrapper = wrapper_path
                .to_str()
                .ok_or_else(|| Error::message("Startup wrapper path is not Unicode"))?;
            let arguments = [
                executable,
                "--config",
                path,
                "--server",
                &config.gateway_url,
                "--no-browser",
                "--recover-lock",
            ];
            let script = format!(
                "#!/bin/sh\ncd {} || exit 1\nexec {}\n",
                shell_quote(parent),
                arguments
                    .iter()
                    .map(|argument| shell_quote(argument))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let mut wrapper_created = Created::write(&wrapper_path, script.as_bytes(), 0o700)?;
            let unit_path = directory.join("rdh.service");
            let unit = format!(
                "[Unit]\nDescription=Remote Device Hub Agent\n[Service]\nExecStart=\"{}\"\nRestart=on-failure\nRestartSec=5\n[Install]\nWantedBy=default.target\n",
                unit_quote(wrapper)
            );
            let mut unit_created = Created::write(&unit_path, unit.as_bytes(), 0o600)?;
            self.checked(&["--user", "daemon-reload"])?;
            File::open(&directory)?.sync_all()?;
            File::open(self.config.parent().unwrap())?.sync_all()?;
            wrapper_created.retain = true;
            unit_created.retain = true;
            Ok(Binding {
                mode: Mode::User,
                name: "rdh.service".into(),
            })
        }
        fn service_authority(&self) -> bool {
            (unsafe { libc::getuid() }) == 0
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        #[test]
        fn native_backend_uses_explicit_unit_without_start_or_stop() {
            let root = std::env::temp_dir().join(format!(
                "rdh-native-startup-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            let executable = root.join("systemctl");
            fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$*\" in *is-enabled*) echo enabled;; esac\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let backend = NativeStartup {
                config: root.join("config"),
                executable: root.join("rdh"),
                config_base: root.clone(),
                systemctl: executable,
                timeout: Duration::from_secs(1),
            };
            let binding = Binding {
                mode: Mode::User,
                name: "rdh.service".into(),
            };
            backend.set(&binding, true).unwrap();
            let calls = fs::read_to_string(root.join("systemctl.calls")).unwrap();
            assert_eq!(
                calls,
                "--no-ask-password --user enable rdh.service\n--user is-enabled rdh.service\n"
            );
            assert!(
                !calls.contains("--now") && !calls.contains("restart") && !calls.contains("stop")
            );
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn new_user_unit_executes_native_binary_with_exact_quoted_arguments() {
            let root = std::env::temp_dir().join(format!(
                "rdh unit '$ %-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            let config = root.join("device ' %.json");
            let value = serde_json::json!({"gateway_url":"http://localhost:8111", "device_id":"fixture",
                "token":"x".repeat(32), "roots":{"work":root}, "allow_write":false});
            crate::storage::write_private_new(&config, &serde_json::to_vec(&value).unwrap())
                .unwrap();
            let executable = root.join("native rdh '$");
            fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let systemctl = root.join("systemctl");
            fs::write(&systemctl, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o700)).unwrap();
            let backend = NativeStartup {
                config: config.clone(),
                executable,
                config_base: root.join("xdg"),
                systemctl,
                timeout: Duration::from_secs(1),
            };
            assert_eq!(
                backend.create_user().unwrap(),
                Binding {
                    mode: Mode::User,
                    name: "rdh.service".into()
                }
            );
            let wrapper = PathBuf::from(format!("{}.startup-run", config.display()));
            assert_eq!(
                fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let output = Command::new(&wrapper).env_clear().output().unwrap();
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!(
                    "--config\n{}\n--server\nhttp://localhost:8111\n--no-browser\n--recover-lock\n",
                    config.display()
                )
            );
            let unit = fs::read_to_string(root.join("xdg/systemd/user/rdh.service")).unwrap();
            assert!(unit.contains("ExecStart=") && unit.contains("%%") && unit.contains("$$"));
            let before = fs::read(&wrapper).unwrap();
            assert!(backend.create_user().is_err());
            assert_eq!(fs::read(&wrapper).unwrap(), before);
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::NativeStartup;

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub(crate) struct NativeStartup;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
impl NativeStartup {
    pub(crate) fn new(_: std::path::PathBuf) -> crate::error::Result<Self> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
}
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
impl crate::startup::StartupBackend for NativeStartup {
    fn enabled(&self, _: &crate::startup::Binding) -> crate::error::Result<bool> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
    fn set(&self, _: &crate::startup::Binding, _: bool) -> crate::error::Result<()> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
    fn create_user(&self) -> crate::error::Result<crate::startup::Binding> {
        Err(crate::error::Error::message(
            "Native startup is unsupported on this platform",
        ))
    }
    fn service_authority(&self) -> bool {
        false
    }
}

#[cfg(all(test, target_os = "macos", not(feature = "experimental-macos-startup")))]
mod macos_default_tests {
    #[test]
    fn startup_requires_explicit_experimental_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-startup-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(super::NativeStartup::new(path.clone()).is_err());
        assert!(crate::startup::STARTUP_OPERATIONS.is_empty());
        assert!(!path.exists());
        assert_eq!(super::DEFAULT_SERVICE_NAME, "com.be-one-click.rdh");
    }
}

#[cfg(all(windows, feature = "experimental-windows-startup"))]
#[path = "startup_native_windows.rs"]
pub(crate) mod windows;
#[cfg(all(windows, feature = "experimental-windows-startup"))]
pub(crate) use windows::NativeStartup;

#[cfg(all(test, windows, not(feature = "experimental-windows-startup")))]
mod windows_default_tests {
    #[test]
    fn startup_requires_the_native_windows_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-startup-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(super::NativeStartup::new(path.clone()).is_err());
        assert!(crate::startup::STARTUP_OPERATIONS.is_empty());
        assert!(!path.exists());
        assert_eq!(super::DEFAULT_SERVICE_NAME, "RemoteDeviceHub");
    }
}
