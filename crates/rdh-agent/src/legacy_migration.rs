//! Explicit offline handoff of a narrowly recognized legacy user service.
//!
//! No service is started or stopped here. A single atomic systemd drop-in is
//! the switch; the original unit, wrapper, installation and pairing remain in
//! place. An authenticated comment makes an interrupted switch reversible.

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CString, OsStr};
    use std::fs::{self, File, Metadata, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Component, Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tokio::process::Command;

    use crate::cli::{Action, parse};
    use crate::config::{DeviceConfig, local_roots};
    use crate::error::{Error, Result};
    use crate::install_lock::{ControllerLock, LaunchBarrier, ManagerLock, current_installation};
    use crate::legacy_inspect::MigrationLayout;
    use crate::profile_lock::ProfileLocks;
    use crate::random;
    use crate::startup::{self, Binding, Mode};
    use crate::systemd_identity::{self, MigrationProperties, MigrationWrapper};

    const DROP_IN: &str = "90-rdh-native-migration.conf";
    const UNDO: &str = "90-rdh-native-migration.undo";
    const HEADER: &str = "# rdh-native-migration ";
    const USAGE: &str = "rdh migrate --offline|--undo --prefix OLD_INSTALL --config ABSOLUTE_PATH --service-unit NAME [--recover-lock]\n\
        Run from a prepared native installation. The legacy service must already be stopped.\n\
        --recover-lock requires confirming that all previous work has stopped.\n\
        These commands do not start, stop, enable or disable a service.\n";
    const LEGACY_LAUNCHER: &[u8] = concat!(
        "#!/bin/sh\n",
        "# Run the bundled Agent with the bundled runtime, independent of PATH.\n",
        "set -eu\n",
        "target=$0\n",
        "while [ -L \"$target\" ]; do\n",
        "  link=$(readlink \"$target\")\n",
        "  case $link in\n",
        "    /*) target=$link ;;\n",
        "    *) target=${target%/*}/$link ;;\n",
        "  esac\n",
        "done\n",
        "dir=$(CDPATH= cd -- \"${target%/*}\" && pwd -P)\n",
        "exec \"$dir/node\" \"$dir/bin/remote-device-hub.mjs\" \"$@\"\n",
    )
    .as_bytes();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Operation {
        Apply,
        Undo,
    }

    struct Options {
        operation: Operation,
        legacy: PathBuf,
        config: PathBuf,
        unit: String,
        recover_lock: bool,
    }

    fn clean_path(path: &Path) -> bool {
        path.is_absolute()
            && path.as_os_str().as_bytes().len() <= 4096
            && path
                .to_str()
                .is_some_and(|s| !s.chars().any(char::is_control))
            && path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
    }

    fn options(args: &[String]) -> Result<Options> {
        let mut operation = None;
        let mut legacy = None;
        let mut config = None;
        let mut unit = None;
        let mut recover_lock = false;
        let mut items = args.iter();
        while let Some(arg) = items.next() {
            match arg.as_str() {
                "--offline" | "--undo" if operation.is_none() => {
                    operation = Some(if arg == "--offline" {
                        Operation::Apply
                    } else {
                        Operation::Undo
                    });
                }
                "--recover-lock" if !recover_lock => recover_lock = true,
                "--prefix" | "--config" | "--service-unit" => {
                    let value = items
                        .next()
                        .filter(|v| !v.is_empty() && !v.starts_with("--"))
                        .ok_or_else(|| Error::message(USAGE))?;
                    match arg.as_str() {
                        "--prefix" if legacy.is_none() => legacy = Some(PathBuf::from(value)),
                        "--config" if config.is_none() => config = Some(PathBuf::from(value)),
                        "--service-unit" if unit.is_none() => unit = Some(value.clone()),
                        _ => return Err(Error::message(USAGE)),
                    }
                }
                _ => return Err(Error::message(USAGE)),
            }
        }
        let (Some(operation), Some(legacy), Some(config), Some(unit)) =
            (operation, legacy, config, unit)
        else {
            return Err(Error::message(USAGE));
        };
        if !clean_path(&legacy) || !clean_path(&config) || !systemd_identity::valid_unit(&unit) {
            return Err(Error::message(USAGE));
        }
        Ok(Options {
            operation,
            legacy,
            config,
            unit,
            recover_lock,
        })
    }

    fn invalid(message: &str) -> Error {
        Error::message(format!("Offline migration refused: {message}"))
    }

    fn cstr(name: &OsStr) -> Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| invalid("invalid path"))
    }

    fn child_dir(parent: &File, name: &OsStr) -> Result<File> {
        let name = cstr(name)?;
        // SAFETY: the descriptor and NUL-terminated single component remain live.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// A private ancestor prevents another account traversing descendants even
    /// if an inner directory uses a permissive umask. Same-UID editors are not
    /// excluded; names and identities are rechecked around the switch.
    fn open_private_tree(path: &Path) -> Result<File> {
        if !clean_path(path) {
            return Err(invalid("unsafe systemd directory path"));
        }
        let mut dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")?;
        let uid = unsafe { libc::geteuid() };
        let mut private = false;
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            dir = child_dir(&dir, name)?;
            let m = dir.metadata()?;
            if m.uid() != uid && m.uid() != 0 {
                return Err(invalid("untrusted directory owner"));
            }
            if m.uid() == uid && m.mode() & 0o077 == 0 {
                private = true;
            }
            if !private && m.mode() & 0o022 != 0 {
                // Root's sticky temporary directories and root's own group are
                // trusted ancestors; neither grants arbitrary users deletion of
                // the subsequently checked private directory.
                let root_sticky = m.uid() == 0 && m.mode() & 0o1000 != 0;
                let root_group = m.uid() == 0 && m.gid() == 0 && m.mode() & 0o002 == 0;
                if !root_sticky && !root_group {
                    return Err(invalid("writable directory ancestor"));
                }
            }
        }
        let m = dir.metadata()?;
        if !private || m.uid() != uid {
            return Err(invalid("systemd directory is not user-controlled"));
        }
        Ok(dir)
    }

    fn same_object(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino()
    }

    fn still_named(path: &Path, held: &File) -> Result<()> {
        let named = open_private_tree(path)?;
        if !same_object(&held.metadata()?, &named.metadata()?) {
            return Err(invalid("systemd directory changed"));
        }
        Ok(())
    }

    fn stat_at(parent: &File, name: &OsStr) -> Result<Option<libc::stat>> {
        let name = cstr(name)?;
        let mut result = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: parent and name are live, result is valid writable storage.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                result.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(Some(unsafe { result.assume_init() }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error.into())
        }
    }

    struct ObservedFile {
        file: File,
        metadata: Metadata,
        bytes: Vec<u8>,
    }

    fn read_at(parent: &File, name: &str, private: bool) -> Result<ObservedFile> {
        let name_c = cstr(OsStr::new(name))?;
        // SAFETY: parent and name are live and openat returns an owned descriptor.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
            || metadata.len() > 128 * 1024
        {
            return Err(invalid("unit or migration file ownership is unsafe"));
        }
        let mut bytes = Vec::new();
        (&mut file).take(128 * 1024 + 1).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let named =
            stat_at(parent, OsStr::new(name))?.ok_or_else(|| invalid("file disappeared"))?;
        if !same_object(&metadata, &after)
            || metadata.len() != after.len()
            || metadata.mtime() != after.mtime()
            || metadata.mtime_nsec() != after.mtime_nsec()
            || metadata.ctime() != after.ctime()
            || metadata.ctime_nsec() != after.ctime_nsec()
            || named.st_dev != metadata.dev()
            || named.st_ino != metadata.ino()
            || named.st_nlink != 1
            || bytes.len() as u64 != metadata.len()
        {
            return Err(invalid("unit or migration file changed"));
        }
        Ok(ObservedFile {
            file,
            metadata,
            bytes,
        })
    }

    fn recheck_file(
        parent: &File,
        name: &str,
        original: &ObservedFile,
        private: bool,
    ) -> Result<()> {
        let again = read_at(parent, name, private)?;
        if !same_object(&original.metadata, &again.metadata)
            || original.bytes != again.bytes
            || !same_object(&original.file.metadata()?, &again.metadata)
        {
            return Err(invalid("unit or migration file changed"));
        }
        Ok(())
    }

    fn quote(value: &str, expand_dollar: bool) -> String {
        let value = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%");
        let value = if expand_dollar {
            value.replace('$', "$$")
        } else {
            value
        };
        format!("\"{value}\"")
    }

    fn legacy_fragment(wrapper: &MigrationWrapper) -> Vec<u8> {
        let start = quote(&wrapper.start, true);
        if wrapper.kind == "installer" {
            format!("[Unit]\nDescription=Remote Device Hub Agent\nAfter=network-online.target\n\n[Service]\nExecStart={start}\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\nWantedBy=default.target\n").into_bytes()
        } else {
            format!("[Unit]\nDescription=Remote Device Hub Agent\n[Service]\nExecStart={start}\nRestart=on-failure\nRestartSec=5\n[Install]\nWantedBy=default.target\n").into_bytes()
        }
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        schema: u8,
        nonce: String,
        legacy: String,
        native: String,
        config: String,
        unit: String,
        wrapper: String,
        proof: String,
    }

    fn executable(native: &Path) -> Result<String> {
        native
            .join("current/rdh")
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("native executable path is not Unicode"))
    }

    fn drop_in(record: &Record, wrapper: &MigrationWrapper) -> Result<Vec<u8>> {
        let native = executable(Path::new(&record.native))?;
        let mut start = quote(&native, false);
        for arg in &wrapper.arguments {
            start.push(' ');
            start.push_str(&quote(arg, true));
        }
        let mut content = format!(
            "{HEADER}{}\n[Service]\nExecStart=\nExecStart={start}\nWorkingDirectory={}\n",
            serde_json::to_string(record)?,
            wrapper.working_directory.replace('%', "%%")
        );
        for (name, value) in &wrapper.environment {
            content.push_str("Environment=");
            content.push_str(&quote(&format!("{name}={value}"), false));
            content.push('\n');
        }
        Ok(content.into_bytes())
    }

    fn read_record(bytes: &[u8]) -> Result<Record> {
        let first = std::str::from_utf8(bytes)
            .map_err(|_| invalid("invalid migration file"))?
            .lines()
            .next()
            .and_then(|s| s.strip_prefix(HEADER))
            .ok_or_else(|| invalid("migration metadata is missing"))?;
        let record: Record =
            serde_json::from_str(first).map_err(|_| invalid("invalid migration metadata"))?;
        if record.schema != 1
            || record.nonce.len() != 32
            || !record
                .nonce
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || record.proof.len() != 64
            || !record
                .proof
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !matches!(record.wrapper.as_str(), "installer" | "profile")
        {
            return Err(invalid("invalid migration metadata"));
        }
        Ok(record)
    }

    fn env_value<'a>(environment: &'a Value, key: &str) -> Result<Option<&'a str>> {
        let mut found = None;
        for entry in environment
            .as_array()
            .ok_or_else(|| invalid("manager environment unavailable"))?
        {
            let entry = entry
                .as_str()
                .ok_or_else(|| invalid("manager environment unavailable"))?;
            let (name, value) = entry
                .split_once('=')
                .ok_or_else(|| invalid("invalid manager environment"))?;
            if name == key && found.replace(value).is_some() {
                return Err(invalid("duplicate manager environment key"));
            }
        }
        Ok(found)
    }

    fn unit_directory(props: &MigrationProperties, unit: &str) -> Result<PathBuf> {
        let base = if let Some(config) =
            env_value(&props.manager_environment, "XDG_CONFIG_HOME")?.filter(|v| !v.is_empty())
        {
            PathBuf::from(config)
        } else {
            PathBuf::from(
                env_value(&props.manager_environment, "HOME")?
                    .ok_or_else(|| invalid("user manager HOME is unavailable"))?,
            )
            .join(".config")
        };
        if !clean_path(&base) {
            return Err(invalid("user manager configuration path is unsafe"));
        }
        let directory = base.join("systemd/user");
        let fragment = systemd_identity::get_str(&props.unit, "FragmentPath")?;
        if Path::new(&fragment) != directory.join(unit)
            || !props.unit_paths.iter().any(|path| path == &directory)
        {
            return Err(invalid(
                "unit is not a persistent user configuration fragment",
            ));
        }
        Ok(directory)
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Loaded {
        Legacy,
        Native,
    }

    fn array_empty(value: &Value, name: &str, kind: &str) -> Result<bool> {
        Ok(systemd_identity::property(value, name, kind)?
            .as_array()
            .ok_or_else(|| invalid("invalid systemd property"))?
            .is_empty())
    }

    fn validate_properties(
        props: &MigrationProperties,
        options: &Options,
        native: &Path,
        wrapper: &MigrationWrapper,
        drop_path: &Path,
        require_reloaded: bool,
    ) -> Result<Loaded> {
        use systemd_identity::{command_data, get_str, property};
        let unit = &props.unit;
        let service = &props.service;
        if get_str(unit, "Id")? != options.unit
            || get_str(unit, "LoadState")? != "loaded"
            || !matches!(
                (
                    get_str(unit, "ActiveState")?.as_str(),
                    get_str(unit, "SubState")?.as_str()
                ),
                ("inactive", "dead") | ("failed", "failed")
            )
            || property(unit, "Transient", "b")?.as_bool() != Some(false)
            || (require_reloaded
                && property(unit, "NeedDaemonReload", "b")?.as_bool() != Some(false))
            || property(unit, "Job", "(uo)")?
                .as_array()
                .is_none_or(|job| job.len() != 2 || job[0] != 0 || job[1] != "/")
            || property(service, "MainPID", "u")?.as_u64() != Some(0)
            || property(service, "ControlPID", "u")?.as_u64() != Some(0)
            || !get_str(service, "ControlGroup")?.is_empty()
        {
            return Err(invalid("service is not stably stopped"));
        }
        let _ = unit_directory(props, &options.unit)?;
        for name in ["OnFailure", "OnSuccess"] {
            if !array_empty(unit, name, "as")? {
                return Err(invalid("unit hooks are unsupported"));
            }
        }
        for name in [
            "ExecCondition",
            "ExecStartPre",
            "ExecStartPost",
            "ExecStop",
            "ExecStopPost",
        ] {
            if !command_data(service, name, "a(sasbttttuii)")?.is_empty()
                || !command_data(service, &format!("{name}Ex"), "a(sasasttttuii)")?.is_empty()
            {
                return Err(invalid("service hooks are unsupported"));
            }
        }
        for (name, kind) in [
            ("EnvironmentFiles", "a(sb)"),
            ("PassEnvironment", "as"),
            ("UnsetEnvironment", "as"),
        ] {
            if !array_empty(service, name, kind)? {
                return Err(invalid("service environment settings are unsupported"));
            }
        }
        if get_str(service, "Type")? != "simple"
            || get_str(service, "Restart")? != "on-failure"
            || get_str(service, "KillMode")?
                != if wrapper.kind == "installer" {
                    "mixed"
                } else {
                    "control-group"
                }
            || !get_str(service, "User")?.is_empty()
            || !get_str(service, "Group")?.is_empty()
            || !get_str(service, "RootDirectory")?.is_empty()
            || !get_str(service, "RootImage")?.is_empty()
            || property(service, "DynamicUser", "b")?.as_bool() != Some(false)
            || property(service, "RemainAfterExit", "b")?.as_bool() != Some(false)
        {
            return Err(invalid(
                "service settings do not match a supported legacy template",
            ));
        }
        let starts = command_data(service, "ExecStart", "a(sasbttttuii)")?;
        let extended = command_data(service, "ExecStartEx", "a(sasasttttuii)")?;
        if starts.len() != 1
            || extended.len() != 1
            || starts[0].len() != 10
            || extended[0].len() != 10
            || starts[0][0] != extended[0][0]
            || starts[0][1] != extended[0][1]
            || starts[0][2].as_bool() != Some(false)
            || !extended[0][2].as_array().is_some_and(Vec::is_empty)
        {
            return Err(invalid("service start command is unsupported"));
        }
        let actual: Vec<&str> = starts[0][1]
            .as_array()
            .ok_or_else(|| invalid("invalid service command"))?
            .iter()
            .map(|v| v.as_str().ok_or_else(|| invalid("invalid service command")))
            .collect::<Result<_>>()?;
        let native_exe = executable(native)?;
        let mut native_args = vec![native_exe.clone()];
        // In systemd's loaded property $$ remains escaped; expansion happens
        // later when the process is executed. The executable path itself is
        // never subject to that substitution.
        native_args.extend(wrapper.arguments.iter().map(|arg| arg.replace('$', "$$")));
        let loaded = if starts[0][0] == wrapper.start && actual == [wrapper.start.as_str()] {
            Loaded::Legacy
        } else if starts[0][0] == native_exe
            && actual
                .iter()
                .copied()
                .eq(native_args.iter().map(String::as_str))
        {
            Loaded::Native
        } else {
            return Err(invalid(
                "loaded start command is not the verified old or new command",
            ));
        };
        let dropins = property(unit, "DropInPaths", "as")?
            .as_array()
            .ok_or_else(|| invalid("drop-in list unavailable"))?;
        let wanted = drop_path
            .to_str()
            .ok_or_else(|| invalid("invalid drop-in path"))?;
        if match loaded {
            Loaded::Legacy => !dropins.is_empty(),
            Loaded::Native => dropins.len() != 1 || dropins[0].as_str() != Some(wanted),
        } {
            return Err(invalid("unexpected loaded systemd drop-in"));
        }
        let environment = property(service, "Environment", "as")?
            .as_array()
            .ok_or_else(|| invalid("service environment unavailable"))?;
        let expected_environment: Vec<String> = match loaded {
            Loaded::Legacy => Vec::new(),
            Loaded::Native => wrapper
                .environment
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect(),
        };
        if !environment
            .iter()
            .map(Value::as_str)
            .eq(expected_environment.iter().map(|s| Some(s.as_str())))
            || (loaded == Loaded::Native
                && get_str(service, "WorkingDirectory")? != wrapper.working_directory)
        {
            return Err(invalid("loaded service environment does not match"));
        }
        Ok(loaded)
    }

    fn observation_fingerprint(prefix: &Path, props: &MigrationProperties) -> Result<String> {
        let mut unit = serde_json::Map::new();
        let mut service = serde_json::Map::new();
        for key in [
            "Id",
            "LoadState",
            "ActiveState",
            "SubState",
            "Job",
            "NeedDaemonReload",
            "Transient",
            "FragmentPath",
            "DropInPaths",
            "OnFailure",
            "OnSuccess",
        ] {
            unit.insert(
                key.into(),
                props.unit.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        for key in [
            "MainPID",
            "ControlPID",
            "ControlGroup",
            "Type",
            "Restart",
            "KillMode",
            "User",
            "Group",
            "RootDirectory",
            "RootImage",
            "DynamicUser",
            "RemainAfterExit",
            "Environment",
            "EnvironmentFiles",
            "PassEnvironment",
            "UnsetEnvironment",
            "WorkingDirectory",
            "ExecStart",
            "ExecStartEx",
            "ExecCondition",
            "ExecConditionEx",
            "ExecStartPre",
            "ExecStartPreEx",
            "ExecStartPost",
            "ExecStartPostEx",
            "ExecStop",
            "ExecStopEx",
            "ExecStopPost",
            "ExecStopPostEx",
        ] {
            service.insert(
                key.into(),
                props.service.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        systemd_identity::definition_hash(
            prefix,
            &json!({"kind":"offline-migration-observation-v1", "unit":unit, "service":service,
            "manager_environment":props.manager_environment, "unit_paths":props.unit_paths}),
        )
    }

    async fn observe(
        options: &Options,
        native: &Path,
        wrapper: &MigrationWrapper,
        drop_path: &Path,
        reloaded: bool,
        expected: Option<Loaded>,
    ) -> Result<(MigrationProperties, Loaded)> {
        let first = systemd_identity::migration_properties(&options.unit).await?;
        let first_state =
            validate_properties(&first, options, native, wrapper, drop_path, reloaded)?;
        let first_hash = observation_fingerprint(native, &first)?;
        let second = systemd_identity::migration_properties(&options.unit).await?;
        let state = validate_properties(&second, options, native, wrapper, drop_path, reloaded)?;
        if state != first_state
            || expected.is_some_and(|want| want != state)
            || first_hash != observation_fingerprint(native, &second)?
        {
            return Err(invalid("service changed during observation"));
        }
        Ok((second, state))
    }

    fn stopped_epoch(props: &MigrationProperties) -> Result<Value> {
        use systemd_identity::property;
        Ok(json!({
            "state":systemd_identity::get_str(&props.unit, "ActiveState")?,
            "substate":systemd_identity::get_str(&props.unit, "SubState")?,
            "changed":property(&props.unit, "StateChangeTimestampMonotonic", "t")?,
            "invocation":property(&props.unit, "InvocationID", "ay")?,
            "restarts":property(&props.service, "NRestarts", "u")?,
            "start":property(&props.service, "ExecMainStartTimestampMonotonic", "t")?,
            "exit":property(&props.service, "ExecMainExitTimestampMonotonic", "t")?,
        }))
    }

    fn verify_wrapper_roots(wrapper: &MigrationWrapper, config: &DeviceConfig) -> Result<()> {
        let Action::Launch(arguments) = parse(&wrapper.arguments, None)? else {
            return Err(invalid("legacy launch arguments are unsupported"));
        };
        if !arguments.roots.is_empty() {
            let roots = arguments
                .roots
                .iter()
                .map(|spec| {
                    let (name, path) = spec
                        .split_once('=')
                        .ok_or_else(|| invalid("legacy root is invalid"))?;
                    let path = Path::new(path);
                    let absolute = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        Path::new(&wrapper.working_directory).join(path)
                    };
                    Ok(format!(
                        "{name}={}",
                        absolute
                            .to_str()
                            .ok_or_else(|| invalid("legacy root is not Unicode"))?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            if local_roots(&roots)? != config.roots {
                return Err(invalid(
                    "legacy root arguments do not match the paired configuration",
                ));
            }
        }
        Ok(())
    }

    fn proof(
        record: &Record,
        props: &MigrationProperties,
        config: &[u8],
        fragment: &[u8],
        wrapper: &MigrationWrapper,
        layout: &MigrationLayout,
    ) -> Result<String> {
        systemd_identity::definition_hash(
            Path::new(&record.native),
            &json!({
                "kind":"legacy-offline-migration-v1", "schema":record.schema, "nonce":record.nonce,
                "legacy":record.legacy, "native":record.native, "config_path":record.config,
                "unit":record.unit, "wrapper_kind":record.wrapper,
                // The HMAC input may contain credentials; only its digest is written.
                "configuration":STANDARD.encode(config), "fragment":STANDARD.encode(fragment),
                "wrapper":STANDARD.encode(&wrapper.contents), "legacy_layout":layout.evidence(),
                "manager_environment":props.manager_environment, "unit_paths":props.unit_paths,
            }),
        )
    }

    fn validate_binding(config: &Path, unit: &str) -> Result<()> {
        let (binding, _) = startup::inspect_sidecar(config)?;
        if binding
            != Some(Binding {
                mode: Mode::User,
                name: unit.into(),
            })
        {
            return Err(invalid(
                "startup binding does not identify the selected user unit",
            ));
        }
        Ok(())
    }

    fn selected_wrapper(
        options: &Options,
        config: &DeviceConfig,
        layout: &MigrationLayout,
        fragment: &[u8],
    ) -> Result<MigrationWrapper> {
        for kind in ["installer", "profile"] {
            if let Ok(wrapper) = systemd_identity::migration_wrapper(
                &options.legacy,
                &options.config,
                &config.gateway_url,
                layout.node(),
                layout.entry(),
                kind,
            ) {
                if legacy_fragment(&wrapper) != fragment {
                    continue;
                }
                if kind == "installer" && layout.launcher() != LEGACY_LAUNCHER {
                    return Err(invalid(
                        "legacy installer launcher is not a recognized version",
                    ));
                }
                verify_wrapper_roots(&wrapper, config)?;
                return Ok(wrapper);
            }
        }
        Err(invalid(
            "unit and legacy wrapper are not a recognized matching template",
        ))
    }

    fn make_drop_dir(parent: &File, path: &Path, name: &str) -> Result<File> {
        let name_c = cstr(OsStr::new(name))?;
        if stat_at(parent, OsStr::new(name))?.is_none() {
            // SAFETY: parent and name are live; mkdirat creates only this name.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            parent.sync_all()?;
        }
        let dir = child_dir(parent, OsStr::new(name))?;
        let m = dir.metadata()?;
        if m.uid() != unsafe { libc::geteuid() } {
            return Err(invalid("drop-in directory owner is unsafe"));
        }
        still_named(path, &dir)?;
        Ok(dir)
    }

    fn no_unexpected_conf(dir: &File) -> Result<()> {
        // /proc/self/fd keeps enumeration anchored to the held directory.
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let name = entry?.file_name();
            if name.as_bytes().ends_with(b".conf") && name != OsStr::new(DROP_IN) {
                return Err(invalid("another drop-in exists for the selected unit"));
            }
        }
        Ok(())
    }

    fn rename_no_replace(dir: &File, from: &str, to: &str) -> Result<()> {
        let from = cstr(OsStr::new(from))?;
        let to = cstr(OsStr::new(to))?;
        // SAFETY: both names and the held directory descriptor are live.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                dir.as_raw_fd(),
                from.as_ptr(),
                dir.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn publish(dir: &File, record: &Record, bytes: &[u8]) -> Result<ObservedFile> {
        if stat_at(dir, OsStr::new(DROP_IN))?.is_some() || stat_at(dir, OsStr::new(UNDO))?.is_some()
        {
            return Err(invalid(
                "a migration file already exists; inspect it and use --undo",
            ));
        }
        no_unexpected_conf(dir)?;
        let temp = format!(".rdh-native-{}.tmp", record.nonce);
        let temp_c = cstr(OsStr::new(&temp))?;
        // SAFETY: dir and the unique NUL-terminated filename are live.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                temp_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let original = file.metadata()?;
        file.write_all(bytes)?;
        file.sync_all()?;
        rename_no_replace(dir, &temp, DROP_IN)?;
        let published = read_at(dir, DROP_IN, true)?;
        if !same_object(&original, &published.metadata) || published.bytes != bytes {
            return Err(invalid("published drop-in identity changed"));
        }
        dir.sync_all()?;
        Ok(published)
    }

    fn unlink_same(dir: &File, name: &str, original: &ObservedFile) -> Result<()> {
        recheck_file(dir, name, original, true)?;
        let name_c = cstr(OsStr::new(name))?;
        // SAFETY: the held directory and NUL-terminated filename are live.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        dir.sync_all()?;
        Ok(())
    }

    async fn daemon_reload() -> Result<()> {
        let mut command = Command::new("/usr/bin/systemctl");
        command
            .args(["--user", "--no-ask-password", "daemon-reload"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .map_err(|_| invalid("systemd reload timed out"))??;
        if !status.success() {
            return Err(invalid("systemd reload failed"));
        }
        Ok(())
    }

    struct Inputs {
        config_dir: File,
        config_name: String,
        config_file: ObservedFile,
        sidecar_name: String,
        sidecar: ObservedFile,
        unit_dir: File,
        unit_path: PathBuf,
        fragment: ObservedFile,
        layout: MigrationLayout,
        wrapper: MigrationWrapper,
    }

    impl Inputs {
        fn recheck(&self, options: &Options, config: &DeviceConfig) -> Result<()> {
            let config_parent = options
                .config
                .parent()
                .ok_or_else(|| invalid("configuration parent missing"))?;
            still_named(config_parent, &self.config_dir)?;
            recheck_file(&self.config_dir, &self.config_name, &self.config_file, true)?;
            recheck_file(&self.config_dir, &self.sidecar_name, &self.sidecar, true)?;
            validate_binding(&options.config, &options.unit)?;
            still_named(&self.unit_path, &self.unit_dir)?;
            recheck_file(&self.unit_dir, &options.unit, &self.fragment, false)?;
            self.layout.recheck()?;
            let again = systemd_identity::migration_wrapper(
                &options.legacy,
                &options.config,
                &config.gateway_url,
                self.layout.node(),
                self.layout.entry(),
                self.wrapper.kind,
            )?;
            if again != self.wrapper {
                return Err(invalid("legacy wrapper changed"));
            }
            Ok(())
        }
    }

    fn uncertain(error: impl std::fmt::Display) -> Error {
        Error::message(format!(
            "{error}. A migration file may exist or a systemd reload may still be in progress; inspect the selected stopped unit and use migrate --undo before retrying."
        ))
    }

    async fn apply(
        options: &Options,
        native: &Path,
        config: &DeviceConfig,
        inputs: &Inputs,
        first: &MigrationProperties,
        drop_path: &Path,
    ) -> Result<Value> {
        let mut record = Record {
            schema: 1,
            nonce: random::hex_token::<16>()?,
            legacy: options
                .legacy
                .to_str()
                .ok_or_else(|| invalid("invalid legacy path"))?
                .into(),
            native: native
                .to_str()
                .ok_or_else(|| invalid("invalid native path"))?
                .into(),
            config: options
                .config
                .to_str()
                .ok_or_else(|| invalid("invalid configuration path"))?
                .into(),
            unit: options.unit.clone(),
            wrapper: inputs.wrapper.kind.into(),
            proof: String::new(),
        };
        record.proof = proof(
            &record,
            first,
            &inputs.config_file.bytes,
            &inputs.fragment.bytes,
            &inputs.wrapper,
            &inputs.layout,
        )?;
        let bytes = drop_in(&record, &inputs.wrapper)?;
        let directory_name = format!("{}.d", options.unit);
        let directory_path = inputs.unit_path.join(&directory_name);
        let dir =
            make_drop_dir(&inputs.unit_dir, &directory_path, &directory_name).map_err(uncertain)?;
        let result = async {
            no_unexpected_conf(&dir)?;
            inputs.recheck(options, config)?;
            let (before, _) = observe(
                options,
                native,
                &inputs.wrapper,
                drop_path,
                true,
                Some(Loaded::Legacy),
            )
            .await?;
            if stopped_epoch(first)? != stopped_epoch(&before)?
                || proof(
                    &record,
                    &before,
                    &inputs.config_file.bytes,
                    &inputs.fragment.bytes,
                    &inputs.wrapper,
                    &inputs.layout,
                )? != record.proof
            {
                return Err(invalid(
                    "service or manager environment changed before publication",
                ));
            }
            still_named(&directory_path, &dir)?;
            let published = publish(&dir, &record, &bytes)?;
            inputs.recheck(options, config)?;
            still_named(&directory_path, &dir)?;
            recheck_file(&dir, DROP_IN, &published, true)?;
            daemon_reload().await?;
            let (after, _) = observe(
                options,
                native,
                &inputs.wrapper,
                drop_path,
                true,
                Some(Loaded::Native),
            )
            .await?;
            inputs.recheck(options, config)?;
            still_named(&directory_path, &dir)?;
            recheck_file(&dir, DROP_IN, &published, true)?;
            if stopped_epoch(&before)? != stopped_epoch(&after)?
                || proof(
                    &record,
                    &after,
                    &inputs.config_file.bytes,
                    &inputs.fragment.bytes,
                    &inputs.wrapper,
                    &inputs.layout,
                )? != record.proof
            {
                return Err(invalid(
                    "service or manager environment changed during publication",
                ));
            }
            Ok(
                json!({"status":"applied", "unit":options.unit, "drop_in":drop_path,
                "service_started_or_stopped":false, "service_readiness":"not_checked",
                "legacy_active_work":"operator_must_confirm_before_migration",
                "network_environment":"device_local_settings_require_review"}),
            )
        }
        .await;
        result.map_err(uncertain)
    }

    async fn undo(
        options: &Options,
        native: &Path,
        config: &DeviceConfig,
        inputs: &Inputs,
        first: &MigrationProperties,
        drop_path: &Path,
    ) -> Result<Value> {
        let directory_path = drop_path
            .parent()
            .ok_or_else(|| invalid("invalid drop-in path"))?;
        let dir = open_private_tree(directory_path)?;
        no_unexpected_conf(&dir)?;
        let conf = stat_at(&dir, OsStr::new(DROP_IN))?.is_some();
        let retired = stat_at(&dir, OsStr::new(UNDO))?.is_some();
        if conf == retired {
            return Err(invalid(
                "expected exactly one managed migration or undo file",
            ));
        }
        let source = if conf { DROP_IN } else { UNDO };
        let old = read_at(&dir, source, true)?;
        let record = read_record(&old.bytes)?;
        if record.legacy != options.legacy.to_str().unwrap_or_default()
            || record.native != native.to_str().unwrap_or_default()
            || record.config != options.config.to_str().unwrap_or_default()
            || record.unit != options.unit
            || record.wrapper != inputs.wrapper.kind
            || old.bytes != drop_in(&record, &inputs.wrapper)?
            || record.proof
                != proof(
                    &record,
                    first,
                    &inputs.config_file.bytes,
                    &inputs.fragment.bytes,
                    &inputs.wrapper,
                    &inputs.layout,
                )?
        {
            return Err(invalid(
                "migration metadata or original service inputs changed",
            ));
        }
        inputs.recheck(options, config)?;
        still_named(directory_path, &dir)?;
        recheck_file(&dir, source, &old, true)?;
        let result = async {
            let retired_file = if conf {
                rename_no_replace(&dir, DROP_IN, UNDO)?;
                let moved = read_at(&dir, UNDO, true)?;
                if !same_object(&old.metadata, &moved.metadata) || old.bytes != moved.bytes {
                    return Err(invalid("migration file changed during undo"));
                }
                dir.sync_all()?;
                moved
            } else {
                old
            };
            inputs.recheck(options, config)?;
            still_named(directory_path, &dir)?;
            recheck_file(&dir, UNDO, &retired_file, true)?;
            daemon_reload().await?;
            let (after, _) = observe(
                options,
                native,
                &inputs.wrapper,
                drop_path,
                true,
                Some(Loaded::Legacy),
            )
            .await?;
            inputs.recheck(options, config)?;
            still_named(directory_path, &dir)?;
            if stopped_epoch(first)? != stopped_epoch(&after)?
                || record.proof
                    != proof(
                        &record,
                        &after,
                        &inputs.config_file.bytes,
                        &inputs.fragment.bytes,
                        &inputs.wrapper,
                        &inputs.layout,
                    )?
            {
                return Err(invalid(
                    "service or manager environment changed during undo",
                ));
            }
            unlink_same(&dir, UNDO, &retired_file)?;
            Ok(json!({"status":"undone", "unit":options.unit,
                "service_started_or_stopped":false, "service_readiness":"not_checked"}))
        }
        .await;
        result.map_err(uncertain)
    }

    async fn inspect_inputs(
        options: &Options,
        native: &Path,
        require_reloaded: bool,
        expected: Option<Loaded>,
    ) -> Result<(Inputs, DeviceConfig, MigrationProperties, PathBuf)> {
        let sidecar_path = startup::sidecar_path(&options.config);
        let config_parent = options
            .config
            .parent()
            .ok_or_else(|| invalid("configuration parent missing"))?;
        let config_dir = open_private_tree(config_parent)?;
        let config_name = options
            .config
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| invalid("invalid configuration filename"))?
            .to_owned();
        let sidecar_name = sidecar_path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| invalid("invalid startup state filename"))?
            .to_owned();
        let config_file = read_at(&config_dir, &config_name, true)?;
        let config = DeviceConfig::parse(serde_json::from_slice(&config_file.bytes)?)?;
        let sidecar = read_at(&config_dir, &sidecar_name, true)?;
        validate_binding(&options.config, &options.unit)?;
        crate::app::validate_config(&[
            "--config".into(),
            options.config.to_string_lossy().into_owned(),
        ])?;

        let preliminary = systemd_identity::migration_properties(&options.unit).await?;
        let unit_path = unit_directory(&preliminary, &options.unit)?;
        let unit_dir = open_private_tree(&unit_path)?;
        let fragment = read_at(&unit_dir, &options.unit, false)?;
        let layout = MigrationLayout::inspect(&options.legacy)?;
        let wrapper = selected_wrapper(options, &config, &layout, &fragment.bytes)?;
        let drop_path = unit_path.join(format!("{}.d", options.unit)).join(DROP_IN);
        let (first, _) = observe(
            options,
            native,
            &wrapper,
            &drop_path,
            require_reloaded,
            expected,
        )
        .await?;
        let inputs = Inputs {
            config_dir,
            config_name,
            config_file,
            sidecar_name,
            sidecar,
            unit_dir,
            unit_path,
            fragment,
            layout,
            wrapper,
        };
        inputs.recheck(options, &config)?;
        Ok((inputs, config, first, drop_path))
    }

    pub(crate) async fn run(args: &[String]) -> Result<()> {
        let options = options(args)?;
        let (native, actual) = current_installation()?
            .ok_or_else(|| invalid("run this command from a prepared native installation"))?;
        if !clean_path(&native)
            || options.config.canonicalize()? != options.config
            || options.legacy.canonicalize()? != options.legacy
            || native == options.legacy
        {
            return Err(invalid(
                "installation and configuration paths must be canonical and distinct",
            ));
        }
        let _controller = ControllerLock::acquire(&native, &actual)?;
        let _barrier = LaunchBarrier::acquire(&native)?;
        let manager = ManagerLock::acquire(&native, &actual, false)?;
        if !manager.profiles()?.contains(&options.config) {
            return Err(invalid(
                "the native installation has not registered this profile",
            ));
        }
        let sidecar_path = startup::sidecar_path(&options.config);
        let lock_targets = [options.config.clone(), sidecar_path];
        if options.recover_lock {
            // A killed controller leaves legacy-compatible lock directories.
            // Observe the exact stopped unit and inputs before explicitly
            // recovering only owners which can be proved dead. Inputs are
            // observed again after reacquiring both locks.
            let _ = inspect_inputs(&options, &native, false, None).await?;
            ProfileLocks::recover(&lock_targets)?;
        }
        let mut profile_locks = ProfileLocks::acquire(&lock_targets)?;
        let expected = (options.operation == Operation::Apply).then_some(Loaded::Legacy);
        let (inputs, config, first, drop_path) = inspect_inputs(
            &options,
            &native,
            options.operation == Operation::Apply,
            expected,
        )
        .await?;
        let result = match options.operation {
            Operation::Apply => {
                apply(&options, &native, &config, &inputs, &first, &drop_path).await
            }
            Operation::Undo => undo(&options, &native, &config, &inputs, &first, &drop_path).await,
        }?;
        profile_locks.release().map_err(uncertain)?;
        println!("{}", result);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        #[test]
        fn launcher_matches_the_known_legacy_archive() {
            assert_eq!(
                crate::config::sha256_hex(LEGACY_LAUNCHER),
                "8e045d68f8d5fadadc890782c40c36abb065754bfbdc17fcd749aed166df5765"
            );
        }

        #[test]
        fn parses_only_explicit_offline_operations() {
            let args: Vec<String> = [
                "--offline",
                "--prefix",
                "/tmp/old",
                "--config",
                "/tmp/device.json",
                "--service-unit",
                "rdh.service",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            assert!(options(&args).is_ok());
            let mut duplicates = args.clone();
            duplicates.push("--undo".into());
            assert!(options(&duplicates).is_err());
            let mut recover = args.clone();
            recover.push("--recover-lock".into());
            assert!(options(&recover).unwrap().recover_lock);
            recover.push("--recover-lock".into());
            assert!(options(&recover).is_err());
            let mut relative = args;
            relative[4] = "relative.json".into();
            assert!(options(&relative).is_err());
        }

        #[test]
        fn publication_never_overwrites_and_retired_file_can_be_verified() {
            let root = std::env::temp_dir().join(format!(
                "rdh-migration-storage-{}",
                random::hex_token::<12>().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let dir = open_private_tree(&root).unwrap();
            let record = Record {
                schema: 1,
                nonce: random::hex_token::<16>().unwrap(),
                legacy: "/tmp/legacy".into(),
                native: "/tmp/native".into(),
                config: "/tmp/config".into(),
                unit: "fixture.service".into(),
                wrapper: "installer".into(),
                proof: "a".repeat(64),
            };
            let bytes = b"verified fixture contents\n";
            let published = publish(&dir, &record, bytes).unwrap();
            assert_eq!(fs::read(root.join(DROP_IN)).unwrap(), bytes);
            assert_eq!(published.metadata.mode() & 0o077, 0);
            assert!(publish(&dir, &record, b"replacement").is_err());
            assert_eq!(fs::read(root.join(DROP_IN)).unwrap(), bytes);

            fs::write(root.join(UNDO), b"unrelated").unwrap();
            assert!(rename_no_replace(&dir, DROP_IN, UNDO).is_err());
            assert_eq!(fs::read(root.join(UNDO)).unwrap(), b"unrelated");
            assert_eq!(fs::read(root.join(DROP_IN)).unwrap(), bytes);
            fs::remove_file(root.join(UNDO)).unwrap();
            rename_no_replace(&dir, DROP_IN, UNDO).unwrap();
            let retired = read_at(&dir, UNDO, true).unwrap();
            assert!(same_object(&published.metadata, &retired.metadata));
            unlink_same(&dir, UNDO, &retired).unwrap();
            assert!(!root.join(UNDO).exists());

            symlink("/tmp/no-such-migration-file", root.join(DROP_IN)).unwrap();
            assert!(publish(&dir, &record, bytes).is_err());
            assert!(read_at(&dir, DROP_IN, true).is_err());
            fs::remove_file(root.join(DROP_IN)).unwrap();
            fs::remove_dir(&root).unwrap();
        }

        #[test]
        fn systemd_executable_and_argument_dollars_have_distinct_rules() {
            assert_eq!(quote("/native$path/rdh", false), "\"/native$path/rdh\"");
            assert_eq!(quote("/config$path/%foo", true), "\"/config$$path/%%foo\"");
            assert_eq!(
                quote("XDG_STATE_HOME=/a$path/%foo", false),
                "\"XDG_STATE_HOME=/a$path/%%foo\""
            );
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::run;
