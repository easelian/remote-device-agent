//! Read-only identity verification for a managed user-systemd Agent service.
//! A unit name by itself is not authority to stop, start, or update a service.

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use hmac::{Hmac, Mac};
    use serde::Serialize;
    use serde_json::{Value, json};
    use sha2::Sha256;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    use crate::cli::{Action, parse};
    use crate::config::{DeviceConfig, same_hub};
    use crate::error::{Error, Result};
    use crate::install_lock::RunningInstall;
    use crate::legacy_inspect::{ScriptIdentity, observe_wrapper};
    use crate::readiness::{Snapshot, probe_profile};
    use crate::storage;

    const BUSCTL: &str = "/usr/bin/busctl";
    const MAX_OUTPUT: usize = 256 * 1024;
    const TIMEOUT: Duration = Duration::from_secs(8);

    #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
    pub struct UnitIdentity {
        pub unit: String,
        pub pid: u32,
        pub invocation_id: String,
        pub control_group: String,
        pub restart: String,
        pub executable: String,
        pub definition_hash: String,
        pub transient: bool,
        pub version: String,
        pub instance_id: String,
        pub ready: bool,
        pub connection_generation: Option<u64>,
        pub quarantined: bool,
    }

    fn invalid() -> Error {
        Error::message("User systemd service identity or settings cannot be verified")
    }

    pub fn valid_unit(name: &str) -> bool {
        name.len() > ".service".len()
            && name.len() <= 255
            && name.ends_with(".service")
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b':')
            })
    }

    async fn busctl(args: &[&str]) -> Result<Value> {
        let mut child = Command::new(BUSCTL)
            .args(["--user", "--json=short"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(invalid)?;
        let (status, output) = tokio::time::timeout(TIMEOUT, async {
            let mut output = Vec::new();
            stdout
                .take(MAX_OUTPUT as u64 + 1)
                .read_to_end(&mut output)
                .await?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, output))
        })
        .await
        .map_err(|_| invalid())??;
        if !status.success() || output.len() > MAX_OUTPUT {
            return Err(invalid());
        }
        serde_json::from_slice(&output).map_err(|_| invalid())
    }

    async fn manager_unit_path(unit: &str, method: &str) -> Result<String> {
        if !valid_unit(unit) {
            return Err(Error::message("Invalid user systemd service name"));
        }
        let value = busctl(&[
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            method,
            "s",
            unit,
        ])
        .await?;
        let data = value["data"]
            .as_array()
            .filter(|data| data.len() == 1)
            .ok_or_else(invalid)?;
        let path = data[0].as_str().ok_or_else(invalid)?;
        if value["type"] != "o"
            || !path.starts_with("/org/freedesktop/systemd1/unit/")
            || !path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_'))
        {
            return Err(invalid());
        }
        Ok(path.into())
    }

    async fn unit_path(unit: &str) -> Result<String> {
        manager_unit_path(unit, "LoadUnit").await
    }

    async fn properties(path: &str, interface: &str) -> Result<Value> {
        let value = busctl(&[
            "call",
            "org.freedesktop.systemd1",
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            "s",
            interface,
        ])
        .await?;
        if value["type"] != "a{sv}" {
            return Err(invalid());
        }
        let data = value["data"]
            .as_array()
            .filter(|data| data.len() == 1 && data[0].is_object())
            .ok_or_else(invalid)?;
        Ok(data[0].clone())
    }

    async fn manager_environment() -> Result<Value> {
        let value = busctl(&[
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "Environment",
        ])
        .await?;
        if value["type"] != "as" || !value["data"].is_array() {
            return Err(invalid());
        }
        Ok(value["data"].clone())
    }

    pub(crate) fn definition_hash(prefix: &Path, value: &Value) -> Result<String> {
        // Local service settings may contain proxy credentials. A keyed
        // digest detects changes without exposing their reusable hash.
        let install_id = storage::read_private(&prefix.join("install-id"))?;
        let key = std::str::from_utf8(&install_id)
            .map_err(|_| invalid())?
            .trim();
        if key.len() != 32
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).map_err(|_| invalid())?;
        mac.update(b"rdh-service-definition-v1\0");
        mac.update(&serde_json::to_vec(value)?);
        Ok(format!("{:x}", mac.finalize().into_bytes()))
    }

    pub(crate) fn property<'a>(values: &'a Value, name: &str, kind: &str) -> Result<&'a Value> {
        let entry = values
            .get(name)
            .filter(|entry| entry["type"] == kind)
            .ok_or_else(invalid)?;
        entry.get("data").ok_or_else(invalid)
    }

    pub(crate) fn command_data(service: &Value, name: &str, kind: &str) -> Result<Vec<Vec<Value>>> {
        let commands = property(service, name, kind)?
            .as_array()
            .ok_or_else(invalid)?;
        commands
            .iter()
            .map(|command| command.as_array().cloned().ok_or_else(invalid))
            .collect()
    }

    fn clean_group(group: &str) -> bool {
        Path::new(group).is_absolute()
            && Path::new(group).components().all(|part| {
                matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    }

    fn read_process_file(pid: u32, name: &str, limit: u64) -> Result<Vec<u8>> {
        let path = format!("/proc/{pid}/{name}");
        let mut value = Vec::new();
        std::fs::File::open(path)?
            .take(limit + 1)
            .read_to_end(&mut value)?;
        if value.len() as u64 > limit {
            return Err(invalid());
        }
        Ok(value)
    }

    fn process_arguments(pid: u32) -> Result<Vec<String>> {
        let bytes = read_process_file(pid, "cmdline", 32 * 1024)?;
        if bytes.is_empty() || bytes.last() != Some(&0) {
            return Err(invalid());
        }
        bytes[..bytes.len() - 1]
            .split(|byte| *byte == 0)
            .map(|part| {
                std::str::from_utf8(part)
                    .map(str::to_owned)
                    .map_err(|_| invalid())
            })
            .collect()
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn expected_wrapper(
        config: &Path,
        executable: &Path,
        gateway: &str,
    ) -> Result<(PathBuf, Vec<String>)> {
        let config = config.to_str().ok_or_else(invalid)?;
        let executable = executable.to_str().ok_or_else(invalid)?;
        let parent = Path::new(config)
            .parent()
            .and_then(Path::to_str)
            .ok_or_else(invalid)?;
        let mut wrapper = PathBuf::from(config).into_os_string();
        wrapper.push(".startup-run");
        let wrapper = PathBuf::from(wrapper);
        let argv: Vec<String> = [
            executable,
            "--config",
            config,
            "--server",
            gateway,
            "--no-browser",
            "--recover-lock",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let script = format!(
            "#!/bin/sh\ncd {} || exit 1\nexec {}\n",
            shell_quote(parent),
            argv.iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let metadata = storage::private_metadata(&wrapper, false)?;
        if metadata.nlink() != 1
            || metadata.mode() & 0o100 == 0
            || storage::read_private(&wrapper)? != script.as_bytes()
        {
            return Err(invalid());
        }
        Ok((wrapper, argv))
    }

    fn validate_arguments(
        argv: &[String],
        executable: &Path,
        config: &Path,
        gateway: &str,
    ) -> Result<()> {
        if argv.first().map(Path::new) != Some(executable) {
            return Err(invalid());
        }
        let has_server = argv[1..]
            .iter()
            .any(|arg| arg == "--server" || arg.starts_with("--server="));
        let has_config = argv[1..]
            .iter()
            .any(|arg| arg == "--config" || arg.starts_with("--config="));
        let Action::Launch(parsed) = parse(&argv[1..], None)? else {
            return Err(invalid());
        };
        if !has_server
            || !has_config
            || parsed.pair_only
            || parsed.name.is_some()
            || !same_hub(&parsed.server, gateway)
            || parsed
                .config
                .as_deref()
                .filter(|path| Path::new(path).is_absolute())
                .and_then(|path| Path::new(path).canonicalize().ok())
                .as_deref()
                != Some(config)
        {
            return Err(invalid());
        }
        if !parsed.roots.is_empty() {
            // A legacy installer may repeat its paired roots in the unit. Only
            // absolute roots which still resolve to the paired values are
            // suitable for a service controller to restart.
            if parsed.roots.iter().any(|root| {
                root.split_once('=')
                    .is_none_or(|(_, path)| !Path::new(path).is_absolute())
            }) || crate::config::local_roots(&parsed.roots)? != DeviceConfig::read(config)?.roots
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    /// systemd retains `$$` in its loaded ExecStart property, then passes a
    /// single dollar to the process. Do not try to reconstruct variable
    /// expansion: only accept literal dollar pairs in argument positions.
    fn literal_systemd_arguments(argv: &[String]) -> Result<Vec<String>> {
        let Some(first) = argv.first() else {
            return Err(invalid());
        };
        let mut result = vec![first.clone()];
        for arg in &argv[1..] {
            let mut expanded = String::with_capacity(arg.len());
            let mut chars = arg.chars();
            while let Some(character) = chars.next() {
                if character == '$' && chars.next() != Some('$') {
                    return Err(invalid());
                }
                expanded.push(character);
            }
            result.push(expanded);
        }
        Ok(result)
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Definition {
        restart: String,
        executable: String,
        definition_hash: String,
        transient: bool,
        process_argv: Vec<String>,
        start_pid: u64,
    }

    pub(crate) fn get_str(values: &Value, key: &str) -> Result<String> {
        Ok(property(values, key, "s")?
            .as_str()
            .ok_or_else(invalid)?
            .to_owned())
    }

    pub(crate) struct MigrationProperties {
        pub unit: Value,
        pub service: Value,
        pub manager_environment: Value,
        pub unit_paths: Vec<PathBuf>,
    }

    /// The caller validates each relevant field. Values may include secrets and
    /// must not be logged or persisted without a keyed digest.
    pub(crate) async fn migration_properties(unit: &str) -> Result<MigrationProperties> {
        let path = unit_path(unit).await?;
        let unit = properties(&path, "org.freedesktop.systemd1.Unit").await?;
        let service = properties(&path, "org.freedesktop.systemd1.Service").await?;
        let manager_environment = manager_environment().await?;
        let paths = busctl(&[
            "get-property",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "UnitPath",
        ])
        .await?;
        if paths["type"] != "as" {
            return Err(invalid());
        }
        let unit_paths = paths["data"]
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|value| value.as_str().map(PathBuf::from).ok_or_else(invalid))
            .collect::<Result<Vec<_>>>()?;
        Ok(MigrationProperties {
            unit,
            service,
            manager_environment,
            unit_paths,
        })
    }

    fn invocation_id(unit_props: &Value) -> Result<String> {
        let invocation = property(unit_props, "InvocationID", "ay")?
            .as_array()
            .filter(|value| value.len() == 16)
            .ok_or_else(invalid)?;
        let mut result = String::new();
        for byte in invocation {
            use std::fmt::Write as _;
            let byte = byte
                .as_u64()
                .filter(|value| *value <= 255)
                .ok_or_else(invalid)?;
            write!(&mut result, "{byte:02x}").map_err(|_| invalid())?;
        }
        if result.bytes().all(|byte| byte == b'0') {
            return Err(invalid());
        }
        Ok(result)
    }

    fn definition(
        unit: &str,
        prefix: &Path,
        config: &Path,
        unit_props: &Value,
        service: &Value,
        manager_env: &Value,
    ) -> Result<Definition> {
        for (name, expected) in [("Id", unit), ("LoadState", "loaded")] {
            if get_str(unit_props, name)? != expected {
                return Err(invalid());
            }
        }
        let job = property(unit_props, "Job", "(uo)")?
            .as_array()
            .ok_or_else(invalid)?;
        if job.len() != 2
            || job[0].as_u64() != Some(0)
            || job[1] != "/"
            || property(unit_props, "NeedDaemonReload", "b")?.as_bool() != Some(false)
        {
            return Err(invalid());
        }
        let transient = property(unit_props, "Transient", "b")?
            .as_bool()
            .ok_or_else(invalid)?;
        let fragment = get_str(unit_props, "FragmentPath")?;
        if !Path::new(&fragment).is_absolute() {
            return Err(invalid());
        }
        for name in ["OnFailure", "OnSuccess"] {
            if !property(unit_props, name, "as")?
                .as_array()
                .is_some_and(Vec::is_empty)
            {
                return Err(invalid());
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
                return Err(invalid());
            }
        }
        let kind = get_str(service, "Type")?;
        let restart = get_str(service, "Restart")?;
        let mut environment = serde_json::Map::new();
        for (name, property_type) in [
            ("Environment", "as"),
            ("EnvironmentFiles", "a(sb)"),
            ("PassEnvironment", "as"),
            ("UnsetEnvironment", "as"),
            ("WorkingDirectory", "s"),
            ("Group", "s"),
            ("SupplementaryGroups", "as"),
            ("PrivateNetwork", "b"),
            ("NetworkNamespacePath", "s"),
            ("ExecSearchPath", "as"),
            ("ProtectSystem", "s"),
            ("ProtectHome", "s"),
            ("NoNewPrivileges", "b"),
            ("BindPaths", "a(ssbt)"),
            ("BindReadOnlyPaths", "a(ssbt)"),
            ("ReadWritePaths", "as"),
            ("ReadOnlyPaths", "as"),
            ("InaccessiblePaths", "as"),
        ] {
            environment.insert(name.into(), property(service, name, property_type)?.clone());
        }
        if !environment["EnvironmentFiles"]
            .as_array()
            .is_some_and(Vec::is_empty)
        {
            return Err(Error::message(
                "Service updates do not support EnvironmentFile; use explicit local service settings",
            ));
        }
        if !matches!(kind.as_str(), "simple" | "exec")
            || !matches!(restart.as_str(), "no" | "on-failure")
            || get_str(service, "KillMode")? != "control-group"
            || !get_str(service, "User")?.is_empty()
            || !get_str(service, "RootDirectory")?.is_empty()
            || !get_str(service, "RootImage")?.is_empty()
            || property(service, "DynamicUser", "b")?.as_bool() != Some(false)
            || property(service, "RemainAfterExit", "b")?.as_bool() != Some(false)
        {
            return Err(invalid());
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
            || starts[0][7].as_u64() != extended[0][7].as_u64()
        {
            return Err(invalid());
        }
        let start_pid = starts[0][7].as_u64().ok_or_else(invalid)?;
        let path = starts[0][0].as_str().ok_or_else(invalid)?;
        let argv: Vec<String> = starts[0][1]
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|arg| arg.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect::<Result<_>>()?;
        if argv.first().is_none_or(|arg| arg != path) {
            return Err(invalid());
        }
        let executable = prefix.join("current/rdh");
        let config_data = DeviceConfig::read(config)?;
        let process_argv = if Path::new(path) == executable {
            let expanded = literal_systemd_arguments(&argv)?;
            validate_arguments(&expanded, &executable, config, &config_data.gateway_url)?;
            expanded
        } else {
            let (wrapper, expected) =
                expected_wrapper(config, &executable, &config_data.gateway_url)?;
            if Path::new(path) != wrapper || argv.len() != 1 {
                return Err(invalid());
            }
            validate_arguments(&expected, &executable, config, &config_data.gateway_url)?;
            expected
        };
        let definition_hash = definition_hash(
            prefix,
            &json!({
                "unit":unit,"fragment":fragment,"transient":transient,
                "type":kind,"restart":restart,"path":path,"argv":argv,"process_argv":process_argv,
                "service_settings":environment,"manager_environment":manager_env
            }),
        )?;
        Ok(Definition {
            restart,
            executable: executable.to_str().ok_or_else(invalid)?.into(),
            definition_hash,
            transient,
            process_argv,
            start_pid,
        })
    }

    fn verify_properties(
        unit: &str,
        prefix: &Path,
        config: &Path,
        snapshot: &Snapshot,
        unit_props: &Value,
        service: &Value,
        manager_env: &Value,
    ) -> Result<UnitIdentity> {
        let definition = definition(unit, prefix, config, unit_props, service, manager_env)?;
        if get_str(unit_props, "ActiveState")? != "active"
            || get_str(unit_props, "SubState")? != "running"
            || property(service, "MainPID", "u")?.as_u64() != Some(u64::from(snapshot.pid))
            // A daemon-reload may discard ExecStart's historical PID while
            // retaining the live MainPID. In that case /proc argv, cgroup,
            // pidfd and the authenticated Agent peer still have to agree.
            || (definition.start_pid != 0 && definition.start_pid != u64::from(snapshot.pid))
            || process_arguments(snapshot.pid)? != definition.process_argv
        {
            return Err(invalid());
        }
        let invocation_id = invocation_id(unit_props)?;
        let group = get_str(service, "ControlGroup")?;
        let cgroup = String::from_utf8(read_process_file(snapshot.pid, "cgroup", 16 * 1024)?)
            .map_err(|_| invalid())?;
        if !clean_group(&group) || cgroup.lines().collect::<Vec<_>>() != [format!("0::{group}")] {
            return Err(invalid());
        }
        Ok(UnitIdentity {
            unit: unit.into(),
            pid: snapshot.pid,
            invocation_id,
            control_group: group,
            restart: definition.restart,
            executable: definition.executable,
            definition_hash: definition.definition_hash,
            transient: definition.transient,
            version: snapshot.version.clone(),
            instance_id: snapshot.instance_id.clone(),
            ready: snapshot.ready,
            connection_generation: snapshot.connection_generation,
            quarantined: snapshot.quarantined,
        })
    }

    /// A point-in-time observation only. The legacy Agent has no authenticated
    /// readiness or idle-control socket, so this is not an update authorization.
    #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
    pub struct LegacyUnitObservation {
        pub unit: String,
        pub state: &'static str,
        pub scope: &'static str,
        pub pid: Option<u32>,
        pub invocation_id: Option<String>,
        pub control_group: Option<String>,
        pub restart: String,
        pub transient: bool,
        pub exec_kind: &'static str,
        pub argument_source: &'static str,
        pub explicit_config_matches: Option<bool>,
        pub explicit_server_matches: Option<bool>,
        pub lock_present: Option<bool>,
        pub recovery_guard_present: Option<bool>,
        pub lock_owner_matches: Option<bool>,
        pub initial_network_environment_keys: Option<Vec<&'static str>>,
        // Compare the actual wrapper identity across snapshots without
        // serializing its contents or a digest of local values.
        #[serde(skip_serializing)]
        wrapper_identity: Option<ScriptIdentity>,
    }

    struct LegacyInputs<'a> {
        unit: &'a str,
        prefix: &'a Path,
        config: &'a Path,
        gateway: &'a str,
        node: &'a std::fs::File,
        entry: &'a std::fs::File,
    }

    fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
        left.dev() == right.dev() && left.ino() == right.ino()
    }

    fn lock_path(config: &Path, suffix: &str) -> PathBuf {
        let mut name = config.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn legacy_lock_present(config: &Path, suffix: &str) -> Option<bool> {
        let path = lock_path(config, suffix);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
            Ok(_) => storage::private_metadata(&path, true).ok().map(|_| true),
            Err(_) => None,
        }
    }

    fn legacy_owner_matches(config: &Path, pid: u32) -> Option<bool> {
        let lock = lock_path(config, ".node-lock");
        storage::private_metadata(&lock, true).ok()?;
        let data = storage::read_private(&lock.join("owner.json")).ok()?;
        if data.len() > 4096 {
            return None;
        }
        let owner: Value = serde_json::from_slice(&data).ok()?;
        let owner_pid = owner["pid"].as_u64().filter(|value| *value > 0)?;
        if owner["nonce"].as_str()?.encode_utf16().count() < 32 {
            return None;
        }
        Some(owner_pid == u64::from(pid))
    }

    fn network_environment_keys(bytes: &[u8]) -> Option<Vec<&'static str>> {
        // Only fixed key names may leave this function. Proxy credentials,
        // NODE_OPTIONS and certificate paths can all contain sensitive values.
        const KEYS: &[&str] = &[
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "NODE_USE_ENV_PROXY",
            "NODE_OPTIONS",
            "NODE_EXTRA_CA_CERTS",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "RDH_USE_ENV_PROXY",
            "RDH_CA_BUNDLE",
        ];
        if !bytes.is_empty() && bytes.last() != Some(&0) {
            return None;
        }
        let mut present = Vec::new();
        for entry in bytes
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let end = entry.iter().position(|byte| *byte == b'=')?;
            let key = &entry[..end];
            if key.is_empty() {
                return None;
            }
            if let Some(name) = KEYS.iter().find(|name| name.as_bytes() == key) {
                if !present.contains(name) {
                    present.push(*name);
                }
            }
        }
        present.sort_unstable();
        Some(present)
    }

    fn legacy_args(inputs: &LegacyInputs<'_>, args: &[String]) -> (Option<bool>, Option<bool>) {
        let has_option = |name: &str| {
            args.iter().any(|arg| {
                arg == name
                    || arg
                        .strip_prefix(name)
                        .is_some_and(|rest| rest.starts_with('='))
            })
        };
        let parsed = match parse(args, None) {
            Ok(Action::Launch(value)) => Some(value),
            _ => None,
        };
        let config = if has_option("--config") {
            parsed.as_ref().and_then(|value| {
                value
                    .config
                    .as_deref()
                    .filter(|path| Path::new(path).is_absolute())
                    .and_then(|path| Path::new(path).canonicalize().ok())
                    .map(|path| path == inputs.config)
            })
        } else {
            None
        };
        let server = if has_option("--server") {
            parsed
                .as_ref()
                .map(|value| same_hub(&value.server, inputs.gateway))
        } else {
            None
        };
        (config, server)
    }

    struct LegacyStart {
        kind: &'static str,
        args: Vec<String>,
    }

    // Decode exactly the single-quoted words emitted by the old installer.
    // Reconstructing the original spelling keeps shell expansions, operators,
    // comments, and arbitrary unquoted fragments out of the accepted format.
    fn literal_word(input: &str) -> Option<(String, &str)> {
        let mut rest = input.strip_prefix('\'')?;
        let mut word = String::new();
        loop {
            let end = rest.find('\'')?;
            word.push_str(&rest[..end]);
            rest = &rest[end + 1..];
            if let Some(next) = rest.strip_prefix("\\''") {
                word.push('\'');
                rest = next;
            } else {
                break;
            }
        }
        if word.chars().any(char::is_control)
            || shell_quote(&word) != input[..input.len() - rest.len()]
        {
            return None;
        }
        Some((word, rest))
    }

    #[derive(Debug, PartialEq, Eq)]
    struct WrapperWords {
        words: Vec<String>,
        environment: Vec<(String, String)>,
        working_directory: String,
    }

    fn wrapper_words(bytes: &[u8], installer: bool, config: &Path) -> Option<WrapperWords> {
        let script = std::str::from_utf8(bytes).ok()?.strip_suffix('\n')?;
        let mut lines = script.split('\n');
        if lines.next()? != "#!/bin/sh" {
            return None;
        }
        let mut line = lines.next()?;
        let mut seen = [false; 2];
        let mut environment = Vec::new();
        while let Some(export) = line.strip_prefix("export ") {
            if !installer {
                return None;
            }
            let (name, value) = export.split_once('=')?;
            let index = match name {
                "XDG_CONFIG_HOME" => 0,
                "XDG_STATE_HOME" => 1,
                _ => return None,
            };
            let (value, rest) = literal_word(value)?;
            if seen[index] || !rest.is_empty() {
                return None;
            }
            seen[index] = true;
            environment.push((name.to_owned(), value));
            line = lines.next()?;
        }
        let (directory, trailing) = literal_word(line.strip_prefix("cd ")?)?;
        if trailing != " || exit 1"
            || !Path::new(&directory).is_absolute()
            || (!installer && Some(Path::new(&directory)) != config.parent())
        {
            return None;
        }
        let mut rest = lines.next()?.strip_prefix("exec ")?;
        if lines.next().is_some() {
            return None;
        }
        let mut words = Vec::new();
        let mut installer_tail = false;
        loop {
            if installer && rest == "--recover-lock" {
                words.push(rest.to_owned());
                installer_tail = true;
                break;
            }
            let (word, remaining) = literal_word(rest)?;
            words.push(word);
            if words.len() > 256 {
                return None;
            }
            if remaining.is_empty() {
                break;
            }
            rest = remaining.strip_prefix(' ')?;
        }
        if words.is_empty() || (installer && !installer_tail) {
            return None;
        }
        Some(WrapperWords {
            words,
            environment,
            working_directory: directory,
        })
    }

    struct LegacyWrapper {
        arguments: Vec<String>,
        identity: ScriptIdentity,
        environment: Vec<(String, String)>,
        working_directory: String,
        contents: Vec<u8>,
    }

    fn legacy_wrapper(inputs: &LegacyInputs<'_>, start: &LegacyStart) -> Option<LegacyWrapper> {
        let installer = match start.kind {
            "installer-wrapper" => true,
            "profile-wrapper" => false,
            _ => return None,
        };
        if start.args.len() != 1 {
            return None;
        }
        let path = Path::new(&start.args[0]);
        let (bytes, identity) =
            observe_wrapper(path.parent()?, path.file_name()?.to_str()?).ok()?;
        let parsed = wrapper_words(&bytes, installer, inputs.config)?;
        let words = &parsed.words;
        let offset = if installer {
            if words.first().map(Path::new) != Some(inputs.prefix.join("current/rdh").as_path()) {
                return None;
            }
            1
        } else {
            if words.len() < 2
                || !Path::new(&words[0]).is_absolute()
                || !Path::new(&words[1]).is_absolute()
                || !same_file(
                    &std::fs::metadata(&words[0]).ok()?,
                    &inputs.node.metadata().ok()?,
                )
                || !same_file(
                    &std::fs::metadata(&words[1]).ok()?,
                    &inputs.entry.metadata().ok()?,
                )
            {
                return None;
            }
            2
        };
        if !matches!(parse(&words[offset..], None), Ok(Action::Launch(args)) if !args.pair_only) {
            return None;
        }
        Some(LegacyWrapper {
            arguments: words[offset..].to_vec(),
            identity,
            environment: parsed.environment,
            working_directory: parsed.working_directory,
            contents: bytes,
        })
    }

    /// A narrowly recognized legacy wrapper, read without executing it.
    /// Its values are kept in memory; they may contain local paths or settings.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct MigrationWrapper {
        pub kind: &'static str,
        pub start: String,
        pub arguments: Vec<String>,
        pub environment: Vec<(String, String)>,
        pub working_directory: String,
        pub contents: Vec<u8>,
        pub identity: ScriptIdentity,
    }

    pub(crate) fn migration_wrapper(
        prefix: &Path,
        config: &Path,
        gateway: &str,
        node: &std::fs::File,
        entry: &std::fs::File,
        kind: &str,
    ) -> Result<MigrationWrapper> {
        let (kind, start) = match kind {
            "installer" => ("installer-wrapper", prefix.join("run-agent")),
            "profile" => {
                let mut path = config.as_os_str().to_os_string();
                path.push(".startup-run");
                ("profile-wrapper", PathBuf::from(path))
            }
            _ => return Err(invalid()),
        };
        let start = start.to_str().ok_or_else(invalid)?.to_owned();
        let inputs = LegacyInputs {
            unit: "",
            prefix,
            config,
            gateway,
            node,
            entry,
        };
        let observed = legacy_wrapper(
            &inputs,
            &LegacyStart {
                kind,
                args: vec![start.clone()],
            },
        )
        .ok_or_else(invalid)?;
        let present = |name: &str| {
            observed
                .arguments
                .iter()
                .filter(|arg| {
                    arg.as_str() == name
                        || arg
                            .strip_prefix(name)
                            .is_some_and(|rest| rest.starts_with('='))
                })
                .count()
        };
        let Action::Launch(parsed) = parse(&observed.arguments, None)? else {
            return Err(invalid());
        };
        // The lock is named from the supplied path, not from an inode. An
        // alias to the same file would therefore not serialize both Agents.
        if present("--config") != 1
            || present("--server") != 1
            || parsed.config.as_deref().map(Path::new) != Some(config)
            || !same_hub(&parsed.server, gateway)
            || parsed.pair_only
            || !Path::new(&observed.working_directory).is_dir()
            // WorkingDirectory= treats its whole unquoted value as a path;
            // trailing whitespace or a backslash is not representable safely.
            || observed.working_directory.ends_with(char::is_whitespace)
            || observed.working_directory.ends_with('\\')
        {
            return Err(Error::message(
                "The legacy wrapper cannot be represented safely for offline migration",
            ));
        }
        Ok(MigrationWrapper {
            kind: if kind == "installer-wrapper" {
                "installer"
            } else {
                "profile"
            },
            start,
            arguments: observed.arguments,
            environment: observed.environment,
            working_directory: observed.working_directory,
            contents: observed.contents,
            identity: observed.identity,
        })
    }

    fn legacy_start(inputs: &LegacyInputs<'_>, service: &Value) -> Result<LegacyStart> {
        let starts = command_data(service, "ExecStart", "a(sasbttttuii)")?;
        if starts.len() != 1 || starts[0].len() != 10 || starts[0][2].as_bool() != Some(false) {
            return Err(invalid());
        }
        let start = Path::new(starts[0][0].as_str().ok_or_else(invalid)?);
        let args: Vec<String> = starts[0][1]
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .map(|arg| arg.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect::<Result<_>>()?;
        if args.first().map(Path::new) != Some(start) {
            return Err(invalid());
        }
        let mut profile_wrapper = inputs.config.as_os_str().to_os_string();
        profile_wrapper.push(".startup-run");
        let node_metadata = inputs.node.metadata()?;
        let kind = if start == inputs.prefix.join("run-agent") {
            "installer-wrapper"
        } else if start == inputs.prefix.join("current/rdh") {
            "current-launcher"
        } else if start == Path::new(&profile_wrapper) {
            "profile-wrapper"
        } else if std::fs::metadata(start)
            .ok()
            .is_some_and(|metadata| same_file(&metadata, &node_metadata))
        {
            "node-executable"
        } else {
            "other"
        };
        Ok(LegacyStart { kind, args })
    }

    fn legacy_properties(
        inputs: &LegacyInputs<'_>,
        unit_props: &Value,
        service: &Value,
        pid: u32,
    ) -> Result<LegacyUnitObservation> {
        if get_str(unit_props, "Id")? != inputs.unit
            || get_str(unit_props, "LoadState")? != "loaded"
            || get_str(unit_props, "ActiveState")? != "active"
            || get_str(unit_props, "SubState")? != "running"
            || property(service, "MainPID", "u")?.as_u64() != Some(u64::from(pid))
            || std::fs::metadata(format!("/proc/{pid}"))?.uid() != unsafe { libc::geteuid() }
            || !same_file(
                &std::fs::metadata(format!("/proc/{pid}/exe"))?,
                &inputs.node.metadata()?,
            )
        {
            return Err(invalid());
        }
        let args = process_arguments(pid)?;
        if args.len() < 2
            || !Path::new(&args[1]).is_absolute()
            || !same_file(&std::fs::metadata(&args[1])?, &inputs.entry.metadata()?)
        {
            return Err(invalid());
        }
        let (explicit_config_matches, explicit_server_matches) = legacy_args(inputs, &args[2..]);
        let start = legacy_start(inputs, service)?;
        let group = get_str(service, "ControlGroup")?;
        let cgroup = String::from_utf8(read_process_file(pid, "cgroup", 16 * 1024)?)
            .map_err(|_| invalid())?;
        if !clean_group(&group) || cgroup.lines().collect::<Vec<_>>() != [format!("0::{group}")] {
            return Err(invalid());
        }
        Ok(LegacyUnitObservation {
            unit: inputs.unit.to_owned(),
            state: "active",
            scope: "running-process",
            pid: Some(pid),
            invocation_id: Some(invocation_id(unit_props)?),
            control_group: Some(group),
            restart: get_str(service, "Restart")?,
            transient: property(unit_props, "Transient", "b")?
                .as_bool()
                .ok_or_else(invalid)?,
            exec_kind: start.kind,
            argument_source: "running-process",
            explicit_config_matches,
            explicit_server_matches,
            lock_present: legacy_lock_present(inputs.config, ".node-lock"),
            recovery_guard_present: legacy_lock_present(inputs.config, ".node-lock.reclaiming"),
            lock_owner_matches: legacy_owner_matches(inputs.config, pid),
            // procfs exposes the process's initial exec environment; this is
            // neither a current effective-network-policy proof nor a record of
            // later in-process environment changes.
            initial_network_environment_keys: read_process_file(pid, "environ", 256 * 1024)
                .ok()
                .and_then(|bytes| network_environment_keys(&bytes)),
            wrapper_identity: None,
        })
    }

    fn legacy_inactive_properties(
        inputs: &LegacyInputs<'_>,
        unit_props: &Value,
        service: &Value,
    ) -> Result<LegacyUnitObservation> {
        let state = get_str(unit_props, "ActiveState")?;
        let substate = get_str(unit_props, "SubState")?;
        let state = match (state.as_str(), substate.as_str()) {
            ("inactive", "dead") => "inactive",
            ("failed", "failed") => "failed",
            _ => {
                return Err(Error::message(
                    "Legacy unit is not in a stable stopped state",
                ));
            }
        };
        let job = property(unit_props, "Job", "(uo)")?
            .as_array()
            .ok_or_else(invalid)?;
        if get_str(unit_props, "Id")? != inputs.unit
            || get_str(unit_props, "LoadState")? != "loaded"
        {
            return Err(Error::message(
                "Legacy unit name or loaded definition does not match",
            ));
        }
        if property(unit_props, "NeedDaemonReload", "b")?.as_bool() != Some(false) {
            return Err(Error::message("Legacy unit requires a systemd reload"));
        }
        if job.len() != 2 || job[0].as_u64() != Some(0) || job[1] != "/" {
            return Err(Error::message("Legacy unit has a pending systemd job"));
        }
        if property(service, "MainPID", "u")?.as_u64() != Some(0) {
            return Err(Error::message("Legacy unit still has a main process"));
        }
        if !get_str(service, "ControlGroup")?.is_empty() {
            return Err(Error::message("Legacy unit still has a control group"));
        }
        let start = legacy_start(inputs, service)
            .map_err(|_| Error::message("Legacy unit start command cannot be inspected"))?;
        let wrapper = legacy_wrapper(inputs, &start);
        let entry_metadata = inputs.entry.metadata()?;
        let direct_args = match start.kind {
            "current-launcher" => Some(&start.args[1..]),
            "node-executable" if start.args.len() >= 2 => {
                let entry = Path::new(&start.args[1]);
                if entry.is_absolute()
                    && std::fs::metadata(entry)
                        .ok()
                        .is_some_and(|metadata| same_file(&metadata, &entry_metadata))
                {
                    Some(&start.args[2..])
                } else {
                    None
                }
            }
            _ => None,
        };
        let (argument_source, launch_args) = if let Some(wrapper) = &wrapper {
            (
                "recognized-wrapper-format",
                Some(wrapper.arguments.as_slice()),
            )
        } else if let Some(args) = direct_args {
            ("unit-definition", Some(args))
        } else {
            ("unverified", None)
        };
        let (explicit_config_matches, explicit_server_matches) =
            launch_args.map_or((None, None), |args| legacy_args(inputs, args));
        Ok(LegacyUnitObservation {
            unit: inputs.unit.to_owned(),
            state,
            scope: "unit-definition",
            pid: None,
            invocation_id: None,
            control_group: None,
            restart: get_str(service, "Restart")?,
            transient: property(unit_props, "Transient", "b")?
                .as_bool()
                .ok_or_else(invalid)?,
            exec_kind: start.kind,
            argument_source,
            explicit_config_matches,
            explicit_server_matches,
            lock_present: legacy_lock_present(inputs.config, ".node-lock"),
            recovery_guard_present: legacy_lock_present(inputs.config, ".node-lock.reclaiming"),
            lock_owner_matches: None,
            initial_network_environment_keys: None,
            wrapper_identity: wrapper.map(|wrapper| wrapper.identity),
        })
    }

    fn legacy_start_fields(service: &Value) -> Result<Vec<Value>> {
        let starts = command_data(service, "ExecStart", "a(sasbttttuii)")?;
        if starts.len() != 1 || starts[0].len() != 10 {
            return Err(invalid());
        }
        // Compare in memory only: arguments can contain local secrets.
        Ok(starts[0][..3].to_vec())
    }

    pub async fn inspect_legacy(
        unit: &str,
        prefix: &Path,
        config: &Path,
        gateway: &str,
        node: &std::fs::File,
        entry: &std::fs::File,
    ) -> Result<LegacyUnitObservation> {
        let inputs = LegacyInputs {
            unit,
            prefix,
            config,
            gateway,
            node,
            entry,
        };
        // LoadUnit reads a definition but does not start the service. systemd
        // may collect an inactive unit again between separate busctl calls;
        // an incomplete observation fails rather than guessing its state.
        let path = unit_path(unit).await?;
        let unit_props = properties(&path, "org.freedesktop.systemd1.Unit").await?;
        let service = properties(&path, "org.freedesktop.systemd1.Service").await?;
        let pid = property(&service, "MainPID", "u")?
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value <= i32::MAX as u32)
            .ok_or_else(invalid)?;
        let process = if pid == 0 {
            None
        } else {
            let process = crate::readiness::process_handle(pid)?;
            crate::readiness::require_live(&process)?;
            Some(process)
        };
        let first = if process.is_some() {
            legacy_properties(&inputs, &unit_props, &service, pid)?
        } else {
            legacy_inactive_properties(&inputs, &unit_props, &service)?
        };
        let start = legacy_start_fields(&service)?;
        let unit_again = properties(&path, "org.freedesktop.systemd1.Unit").await?;
        let service_again = properties(&path, "org.freedesktop.systemd1.Service").await?;
        let after = if process.is_some() {
            legacy_properties(&inputs, &unit_again, &service_again, pid)?
        } else {
            legacy_inactive_properties(&inputs, &unit_again, &service_again)?
        };
        if let Some(process) = process.as_ref() {
            crate::readiness::require_live(process)?;
        }
        if first != after || start != legacy_start_fields(&service_again)? {
            return Err(Error::message("Legacy service changed during inspection"));
        }
        Ok(after)
    }

    /// Recheck an inactive, restartable unit before asking systemd to start it.
    /// The definition is compared to the one recorded before the old stop.
    pub async fn inspect_stopped(
        prefix: &Path,
        config: &Path,
        unit: &str,
        expected_hash: &str,
    ) -> Result<()> {
        let config = config.canonicalize()?;
        let path = unit_path(unit).await?;
        for _ in 0..2 {
            let unit_props = properties(&path, "org.freedesktop.systemd1.Unit").await?;
            let service = properties(&path, "org.freedesktop.systemd1.Service").await?;
            let manager_env = manager_environment().await?;
            let definition =
                definition(unit, prefix, &config, &unit_props, &service, &manager_env)?;
            let state = get_str(&unit_props, "ActiveState")?;
            let substate = get_str(&unit_props, "SubState")?;
            if definition.transient
                || definition.definition_hash != expected_hash
                || !matches!(
                    (state.as_str(), substate.as_str()),
                    ("inactive", "dead") | ("failed", "failed")
                )
                || property(&service, "MainPID", "u")?.as_u64() != Some(0)
                || !get_str(&service, "ControlGroup")?.is_empty()
            {
                return Err(Error::message(
                    "The stopped service definition or state changed",
                ));
            }
        }
        Ok(())
    }

    /// Verify the same ready Agent before and after inspecting structured
    /// systemd properties and /proc. This has no service side effects.
    pub async fn inspect(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        unit: &str,
    ) -> Result<UnitIdentity> {
        let (_, identity) = inspect_state(guard, prefix, config, unit, true).await?;
        Ok(identity)
    }

    /// The rollback path may need to identify a trial that never reached the
    /// Hub. It remains quarantined and must still prove its local peer identity.
    pub async fn inspect_trial(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        unit: &str,
    ) -> Result<(Snapshot, UnitIdentity)> {
        let (snapshot, identity) = inspect_state(guard, prefix, config, unit, false).await?;
        if !snapshot.quarantined {
            return Err(Error::message(
                "The service instance is not a quarantined trial",
            ));
        }
        Ok((snapshot, identity))
    }

    async fn inspect_state(
        guard: &RunningInstall,
        prefix: &Path,
        config: &Path,
        unit: &str,
        require_ready: bool,
    ) -> Result<(Snapshot, UnitIdentity)> {
        if !config.is_absolute() {
            return Err(Error::message("Service profile path must be absolute"));
        }
        let config = config.canonicalize()?;
        let first = probe_profile(guard, prefix, &config).await?;
        if require_ready && !first.ready {
            return Err(Error::message("The service Agent is not Hub-ready"));
        }
        let path = unit_path(unit).await?;
        let unit_props = properties(&path, "org.freedesktop.systemd1.Unit").await?;
        let service = properties(&path, "org.freedesktop.systemd1.Service").await?;
        let manager_env = manager_environment().await?;
        let identity = verify_properties(
            unit,
            prefix,
            &config,
            &first,
            &unit_props,
            &service,
            &manager_env,
        )?;
        let unit_again = properties(&path, "org.freedesktop.systemd1.Unit").await?;
        let service_again = properties(&path, "org.freedesktop.systemd1.Service").await?;
        let manager_again = manager_environment().await?;
        let after = probe_profile(guard, prefix, &config).await?;
        let verified = verify_properties(
            unit,
            prefix,
            &config,
            &after,
            &unit_again,
            &service_again,
            &manager_again,
        )?;
        if identity != verified
            || (require_ready && !after.ready)
            || first.pid != after.pid
            || first.instance_id != after.instance_id
            || first.version != after.version
            || first.connection_generation != after.connection_generation
            || first.quarantined != after.quarantined
        {
            return Err(Error::message("Service identity changed during inspection"));
        }
        Ok((after, identity))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn legacy_network_environment_reports_only_fixed_names() {
            let environment = b"HTTP_PROXY=http://user:secret@host\0NODE_OPTIONS=--no-warnings\0\
                NODE_EXTRA_CA_CERTS=/private/cert.pem\0UNRELATED=secret\0HTTP_PROXY=other\0";
            let names = network_environment_keys(environment).unwrap();
            assert_eq!(names, ["HTTP_PROXY", "NODE_EXTRA_CA_CERTS", "NODE_OPTIONS"]);
            assert!(network_environment_keys(b"HTTP_PROXY=value").is_none());
            assert!(network_environment_keys(b"MALFORMED\0").is_none());
            assert_eq!(network_environment_keys(b"").unwrap(), Vec::<&str>::new());
        }

        #[test]
        fn legacy_wrapper_format_accepts_literals_and_rejects_shell_commands() {
            let config = Path::new("/tmp/device's folder/device.json");
            let args = [
                "/tmp/old install/current/rdh",
                "--no-browser",
                "--config",
                config.to_str().unwrap(),
                "--server",
                "http://127.0.0.1:9",
                "--root",
                "work=/tmp/$(literal)'s files",
            ];
            let exec = args
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            let script = format!(
                "#!/bin/sh\nexport XDG_CONFIG_HOME={}\nexport XDG_STATE_HOME={}\ncd {} || exit 1\nexec {exec} --recover-lock\n",
                shell_quote("/tmp/user's config"),
                shell_quote("/tmp/state"),
                shell_quote("/tmp/user's home"),
            );
            let mut expected: Vec<String> = args.iter().map(|arg| (*arg).into()).collect();
            expected.push("--recover-lock".into());
            let parsed = wrapper_words(script.as_bytes(), true, config).unwrap();
            assert_eq!(parsed.words, expected);
            assert_eq!(parsed.working_directory, "/tmp/user's home");
            assert_eq!(
                parsed.environment,
                [
                    ("XDG_CONFIG_HOME".into(), "/tmp/user's config".into()),
                    ("XDG_STATE_HOME".into(), "/tmp/state".into()),
                ]
            );
            for changed in [
                script.replace("export XDG_CONFIG_HOME=", "export UNEXPECTED="),
                script.replace(" || exit 1", "; touch /tmp/side-effect"),
                script.replace(
                    " --recover-lock\n",
                    " --recover-lock; touch /tmp/side-effect\n",
                ),
                script.replace("'--no-browser'", "$(touch /tmp/side-effect)"),
                format!("{script}touch /tmp/side-effect\n"),
                script.trim_end().to_owned(),
            ] {
                assert!(wrapper_words(changed.as_bytes(), true, config).is_none());
            }
            let profile = format!(
                "#!/bin/sh\ncd {} || exit 1\nexec {} {} {} {}\n",
                shell_quote(config.parent().unwrap().to_str().unwrap()),
                shell_quote("/tmp/old/node"),
                shell_quote("/tmp/old/entry.mjs"),
                shell_quote("--config"),
                shell_quote(config.to_str().unwrap()),
            );
            assert_eq!(
                wrapper_words(profile.as_bytes(), false, config)
                    .unwrap()
                    .words
                    .len(),
                4
            );
            assert!(wrapper_words(profile.as_bytes(), true, config).is_none());
        }

        #[test]
        fn unresolved_legacy_config_arguments_remain_unverified() {
            let root = std::env::temp_dir().join(format!(
                "rdh-legacy-args-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            let config = root.join("device.json");
            let other = root.join("other.json");
            std::fs::write(&config, b"{}").unwrap();
            std::fs::write(&other, b"{}").unwrap();
            let file = std::fs::File::open(&config).unwrap();
            let inputs = LegacyInputs {
                unit: "rdh.service",
                prefix: &root,
                config: &config,
                gateway: "http://127.0.0.1:9",
                node: &file,
                entry: &file,
            };
            let arguments = |path: &str| {
                vec![
                    "--config".into(),
                    path.into(),
                    "--server".into(),
                    inputs.gateway.into(),
                ]
            };
            assert_eq!(
                legacy_args(&inputs, &arguments("device.json")),
                (None, Some(true))
            );
            assert_eq!(
                legacy_args(&inputs, &arguments(root.join("missing").to_str().unwrap())),
                (None, Some(true))
            );
            assert_eq!(
                legacy_args(&inputs, &arguments(config.to_str().unwrap())),
                (Some(true), Some(true))
            );
            assert_eq!(
                legacy_args(&inputs, &arguments(other.to_str().unwrap())),
                (Some(false), Some(true))
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn names_and_service_arguments_are_conservative() {
            assert!(valid_unit("rdh-123.service"));
            for name in [
                "../rdh.service",
                "rdh.service\n",
                "rdh.socket",
                ".service",
                "--other.service",
            ] {
                assert!(!valid_unit(name));
            }
            let root = std::env::temp_dir().join(format!(
                "rdh-unit-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&root).unwrap();
            let config = root.join("device.json");
            std::fs::write(&config, b"{}").unwrap();
            let exe = root.join("current/rdh");
            let argv = vec![
                exe.to_str().unwrap().into(),
                "--server".into(),
                "http://127.0.0.1:9".into(),
                "--config".into(),
                config.to_str().unwrap().into(),
                "--no-browser".into(),
            ];
            assert!(validate_arguments(&argv, &exe, &config, "http://127.0.0.1:9").is_ok());
            let mut pair = argv.clone();
            pair.push("--pair-only".into());
            assert!(validate_arguments(&pair, &exe, &config, "http://127.0.0.1:9").is_err());
            let mut wrong = argv;
            wrong[2] = "http://127.0.0.1:10".into();
            assert!(validate_arguments(&wrong, &exe, &config, "http://127.0.0.1:9").is_err());
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(
                &config,
                serde_json::to_vec(&json!({
                    "gateway_url":"http://127.0.0.1:9", "device_id":"fixture",
                    "token":"x".repeat(32), "roots":{"work":root}
                }))
                .unwrap(),
            )
            .unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
            let mut roots = wrong;
            roots[2] = "http://127.0.0.1:9".into();
            roots.extend(["--root".into(), format!("work={}", root.display())]);
            assert!(validate_arguments(&roots, &exe, &config, "http://127.0.0.1:9").is_ok());
            *roots.last_mut().unwrap() = "work=.".into();
            assert!(validate_arguments(&roots, &exe, &config, "http://127.0.0.1:9").is_err());
            *roots.last_mut().unwrap() = "work=/".into();
            assert!(validate_arguments(&roots, &exe, &config, "http://127.0.0.1:9").is_err());
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn systemd_literal_dollars_match_runtime_arguments_without_expanding_variables() {
            let argv = vec![
                "/tmp/native$prefix/rdh".into(),
                "--config".into(),
                "/tmp/a$$b$$$$c.json".into(),
            ];
            assert_eq!(
                literal_systemd_arguments(&argv).unwrap(),
                ["/tmp/native$prefix/rdh", "--config", "/tmp/a$b$$c.json"]
            );
            for value in ["$VARIABLE", "${VARIABLE}", "prefix$VARIABLE", "trailing$"] {
                assert!(literal_systemd_arguments(&["/tmp/rdh".into(), value.into()]).is_err());
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    LegacyUnitObservation, UnitIdentity, inspect, inspect_legacy, inspect_stopped, inspect_trial,
    valid_unit,
};
#[cfg(target_os = "linux")]
pub(crate) use linux::{
    MigrationProperties, MigrationWrapper, command_data, definition_hash, get_str,
    migration_properties, migration_wrapper, property,
};
