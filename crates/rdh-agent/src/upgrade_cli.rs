//! Native development install and offline update commands.

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
))]
mod native {
    #[cfg(unix)]
    use std::fs::Metadata;
    use std::fs::OpenOptions;
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    #[cfg(windows)]
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Component, Path, PathBuf};

    use url::Url;

    use crate::error::{Error, Result};
    use crate::install_lock::{ManagerLock, RunningInstall, current_installation, pointer};
    use crate::native_install;
    use crate::network::Network;
    use crate::profile_lock::ProfileLocks;
    use crate::storage;
    use crate::update::{Artifact, Channel, Decision, MAX_BINARY_BYTES};

    const INDEX_LIMIT: usize = 256 * 1024;
    const DEFAULT_INDEX: &str =
        "https://raw.githubusercontent.com/easelian/remote-device-agent/main/channels";
    #[cfg(target_os = "linux")]
    const USAGE: &str = "rdh upgrade [--channel stable|canary] [--index URL|FILE] [--check|--dry-run] [--force] [--no-restart]\n\
        rdh upgrade --recover [--recover-lock]\n\
        rdh upgrade --service-unit NAME --service-config ABSOLUTE_PATH [--index URL|FILE] [--channel stable|canary] [--force]\n\
        rdh upgrade --recover-service [--recover-lock]\n\
        rdh upgrade --list-profiles | --forget-profile ABSOLUTE_PATH\n\
        rdh upgrade --probe-profile ABSOLUTE_PATH\n\
        rdh upgrade --stop-if-idle ABSOLUTE_PATH\n\
        rdh upgrade --inspect-service NAME --service-config ABSOLUTE_PATH\n\
        rdh rollback --no-restart\n\n\
        Service updates are experimental and require a verified user-systemd unit.\n\
        The local probe reports the Agent's latest observed Hub connection state.\n\
        Offline updates require all managed Agents to be stopped.\n";

    #[cfg(not(target_os = "linux"))]
    const USAGE: &str = "rdh upgrade [--channel stable|canary] [--index URL|FILE] [--check|--dry-run] [--force] [--no-restart]\n\
        rdh upgrade --recover [--recover-lock]\n\
        rdh upgrade --list-profiles | --forget-profile ABSOLUTE_PATH\n\
        rdh rollback --no-restart\n\n\
        Offline updates require all managed Agents to be stopped. Restart and verify the Agent after switching.\n";

    #[derive(Default)]
    struct Options {
        channel: Option<String>,
        index: Option<String>,
        check: bool,
        dry_run: bool,
        force: bool,
        no_restart: bool,
        recover: bool,
        recover_service: bool,
        recover_lock: bool,
        list_profiles: bool,
        forget_profile: Option<String>,
        probe_profile: Option<String>,
        stop_if_idle: Option<String>,
        inspect_service: Option<String>,
        service_unit: Option<String>,
        service_config: Option<String>,
    }
    fn options(args: &[String]) -> Result<Options> {
        let mut result = Options::default();
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--channel" | "--index" | "--forget-profile" | "--probe-profile"
                | "--stop-if-idle" | "--inspect-service" | "--service-unit"
                | "--service-config" => {
                    let name = args[index].clone();
                    index += 1;
                    let value = args
                        .get(index)
                        .filter(|value| !value.is_empty() && !value.starts_with("--"))
                        .ok_or_else(|| Error::message(format!("{name} requires a value")))?
                        .clone();
                    if name == "--channel" {
                        if !["stable", "canary"].contains(&value.as_str()) {
                            return Err(Error::message("--channel must be stable or canary"));
                        }
                        result.channel = Some(value);
                    } else if name == "--index" {
                        result.index = Some(value);
                    } else if name == "--forget-profile" {
                        result.forget_profile = Some(value);
                    } else if name == "--probe-profile" {
                        result.probe_profile = Some(value);
                    } else if name == "--stop-if-idle" {
                        result.stop_if_idle = Some(value);
                    } else if name == "--inspect-service" {
                        result.inspect_service = Some(value);
                    } else if name == "--service-unit" {
                        result.service_unit = Some(value);
                    } else {
                        result.service_config = Some(value);
                    }
                }
                "--check" => result.check = true,
                "--dry-run" => result.dry_run = true,
                "--force" => result.force = true,
                "--no-restart" => result.no_restart = true,
                "--recover" => result.recover = true,
                "--recover-service" => result.recover_service = true,
                "--recover-lock" => result.recover_lock = true,
                "--list-profiles" => result.list_profiles = true,
                value => return Err(Error::message(format!("Unknown argument: {value}"))),
            }
            index += 1;
        }
        if result.recover
            && (result.channel.is_some()
                || result.index.is_some()
                || result.check
                || result.dry_run
                || result.force
                || result.no_restart
                || result.list_profiles
                || result.forget_profile.is_some()
                || result.probe_profile.is_some()
                || result.stop_if_idle.is_some()
                || result.inspect_service.is_some()
                || result.service_unit.is_some()
                || result.recover_service
                || result.service_config.is_some())
        {
            return Err(Error::message(
                "--recover accepts only the optional --recover-lock",
            ));
        }
        if result.recover_service
            && (result.channel.is_some()
                || result.index.is_some()
                || result.check
                || result.dry_run
                || result.force
                || result.no_restart
                || result.recover
                || result.list_profiles
                || result.forget_profile.is_some()
                || result.probe_profile.is_some()
                || result.stop_if_idle.is_some()
                || result.inspect_service.is_some()
                || result.service_unit.is_some()
                || result.service_config.is_some())
        {
            return Err(Error::message(
                "--recover-service accepts only the optional --recover-lock",
            ));
        }
        let offline_recovery =
            cfg!(windows) && result.no_restart && !result.check && !result.dry_run;
        if result.recover_lock && !(result.recover || result.recover_service || offline_recovery) {
            return Err(Error::message(
                "--recover-lock requires recovery, or a Windows offline update after confirming the prior Agent and its work have stopped",
            ));
        }
        let local_commands = usize::from(result.list_profiles)
            + usize::from(result.forget_profile.is_some())
            + usize::from(result.probe_profile.is_some())
            + usize::from(result.stop_if_idle.is_some())
            + usize::from(result.inspect_service.is_some());
        let service_selectors = usize::from(result.inspect_service.is_some())
            + usize::from(result.service_unit.is_some());
        if service_selectors > 1 || (service_selectors == 1) != result.service_config.is_some() {
            return Err(Error::message(
                "Choose --inspect-service or --service-unit together with --service-config",
            ));
        }
        if result.service_unit.is_some() && (result.check || result.dry_run || result.no_restart) {
            return Err(Error::message(
                "A service update cannot be combined with --check, --dry-run, or --no-restart",
            ));
        }
        if local_commands > 1
            || (local_commands > 0
                && (result.channel.is_some()
                    || result.index.is_some()
                    || result.check
                    || result.dry_run
                    || result.force
                    || result.no_restart
                    || result.recover
                    || result.recover_service
                    || result.service_unit.is_some()))
        {
            return Err(Error::message(
                "Profile catalog commands do not accept other update options",
            ));
        }
        Ok(result)
    }
    enum Location {
        Local(PathBuf),
        Remote(Url),
    }
    fn location(value: &str) -> Result<Location> {
        if value.starts_with("https:") || value.starts_with("http:") {
            return Ok(Location::Remote(
                Url::parse(value).map_err(|_| Error::message("Invalid native update URL"))?,
            ));
        }
        if value.starts_with("file:") {
            let url = Url::parse(value).map_err(|_| Error::message("Invalid local update URL"))?;
            return Ok(Location::Local(
                url.to_file_path()
                    .map_err(|_| Error::message("Invalid local update URL"))?,
            ));
        }
        if value.contains("://") {
            return Err(Error::message("Unsupported native update URL"));
        }
        let path = PathBuf::from(value);
        Ok(Location::Local(if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        }))
    }
    fn resolve(base: &Location, value: &str) -> Result<Location> {
        if value.starts_with("http:") || value.starts_with("https:") {
            let target = location(value)?;
            if matches!((base, &target), (Location::Remote(origin), Location::Remote(target))
                if origin.scheme() == "https" && target.scheme() != "https")
            {
                return Err(Error::message(
                    "An HTTPS release index cannot select an HTTP artifact",
                ));
            }
            return Ok(target);
        }
        if value.starts_with("file:") {
            if matches!(base, Location::Remote(_)) {
                return Err(Error::message(
                    "Remote indexes cannot reference local files",
                ));
            }
            return location(value);
        }
        if value.contains("://") {
            return Err(Error::message("Unsupported native artifact URL"));
        }
        match base {
            Location::Remote(url) => {
                Ok(Location::Remote(url.join(value).map_err(|_| {
                    Error::message("Invalid native artifact URL")
                })?))
            }
            Location::Local(path) => Ok(Location::Local(
                path.parent()
                    .ok_or_else(|| Error::message("Invalid local index path"))?
                    .join(value),
            )),
        }
    }
    async fn read(location: &Location, limit: usize) -> Result<Vec<u8>> {
        match location {
            Location::Remote(url) => Network::from_env()?.get_update(url, limit).await,
            Location::Local(path) => {
                #[cfg(unix)]
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(path)?;
                #[cfg(windows)]
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(
                        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
                    )
                    .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
                    .open(path)?;
                #[cfg(windows)]
                if file.metadata()?.file_attributes()
                    & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                    != 0
                {
                    return Err(Error::message(
                        "Local native update input cannot be a reparse point",
                    ));
                }
                if !file.metadata()?.is_file() {
                    return Err(Error::message(
                        "Local native update input must be a regular file",
                    ));
                }
                let mut bytes = Vec::new();
                file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > limit {
                    return Err(Error::message(
                        "Local native update input exceeds its size limit",
                    ));
                }
                Ok(bytes)
            }
        }
    }
    fn platform() -> Result<&'static str> {
        match (
            std::env::consts::OS,
            std::env::consts::ARCH,
            cfg!(target_env = "musl"),
        ) {
            ("linux", "x86_64", false) => Ok("linux-x64-gnu"),
            ("linux", "x86_64", true) => Ok("linux-x64-musl"),
            ("linux", "aarch64", false) => Ok("linux-arm64-gnu"),
            ("linux", "aarch64", true) => Ok("linux-arm64-musl"),
            ("macos", "x86_64", _) => Ok("darwin-x64"),
            ("macos", "aarch64", _) => Ok("darwin-arm64"),
            ("windows", "x86_64", _) => Ok("win32-x64"),
            ("windows", "aarch64", _) => Ok("win32-arm64"),
            _ => Err(Error::message("Native update platform is unsupported")),
        }
    }
    fn install_id(prefix: &Path) -> Result<String> {
        let data = storage::read_private(&prefix.join("install-id"))?;
        let value = std::str::from_utf8(&data)
            .map_err(|_| Error::message("Invalid native installation ID"))?
            .trim();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::message("Invalid native installation ID"));
        }
        Ok(value.into())
    }
    #[cfg(unix)]
    fn config_snapshot(path: &Path) -> Result<Metadata> {
        let metadata = storage::private_metadata(path, false)?;
        if metadata.nlink() != 1 {
            return Err(Error::message(
                "Existing configuration must not have hard links",
            ));
        }
        Ok(metadata)
    }
    #[cfg(unix)]
    fn same_config(left: &Metadata, right: &Metadata) -> bool {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.nlink() == right.nlink()
            && left.len() == right.len()
            && left.mode() == right.mode()
            && left.uid() == right.uid()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }

    #[cfg(windows)]
    struct ConfigSnapshot {
        object: crate::install_lock::windows::Object,
        digest: String,
    }
    #[cfg(windows)]
    fn config_snapshot(path: &Path) -> Result<ConfigSnapshot> {
        let object = crate::install_lock::windows::private_object(path, false)?;
        let digest = crate::config::sha256_hex(&storage::read_private(path)?);
        Ok(ConfigSnapshot { object, digest })
    }
    #[cfg(windows)]
    fn same_config(left: &ConfigSnapshot, right: &ConfigSnapshot) -> bool {
        use crate::storage::windows::file_identity;
        matches!((file_identity(&left.object.file), file_identity(&right.object.file)), (Ok(left), Ok(right)) if left == right)
            && left.digest == right.digest
    }

    fn install_with_profile(prefix: &Path, selected: &Path) -> Result<PathBuf> {
        if !selected.is_absolute()
            || !selected.components().all(|part| {
                matches!(
                    part,
                    Component::Prefix(_) | Component::RootDir | Component::Normal(_)
                )
            })
        {
            return Err(Error::message(
                "Existing configuration path must be absolute",
            ));
        }
        let original = config_snapshot(selected)?;
        let config = selected.canonicalize()?;
        if !same_config(&original, &config_snapshot(&config)?) {
            return Err(Error::message(
                "Existing configuration changed before validation",
            ));
        }
        let mut locks = ProfileLocks::acquire(std::slice::from_ref(&config))?;
        let result = (|| {
            if !same_config(&original, &config_snapshot(selected)?)
                || !same_config(&original, &config_snapshot(&config)?)
            {
                return Err(Error::message(
                    "Existing configuration changed before validation",
                ));
            }
            let path = config
                .to_str()
                .ok_or_else(|| Error::message("Config path must be Unicode"))?;
            crate::app::validate_config(&["--config".into(), path.into()])?;
            if !same_config(&original, &config_snapshot(selected)?)
                || !same_config(&original, &config_snapshot(&config)?)
            {
                return Err(Error::message(
                    "Existing configuration changed during validation",
                ));
            }
            let installed = native_install::install_for_locked_profile(prefix, &config)?;
            let unchanged = config_snapshot(selected)
                .ok()
                .is_some_and(|metadata| same_config(&original, &metadata))
                && config_snapshot(&config)
                    .ok()
                    .is_some_and(|metadata| same_config(&original, &metadata));
            if !unchanged {
                return Err(Error::message(format!(
                    "Native installation was created at {}, but the existing configuration changed during preparation; validate it before use",
                    installed.display()
                )));
            }
            Ok(installed)
        })();
        let released = locks.release();
        match (result, released) {
            (Ok(installed), Ok(())) => Ok(installed),
            (Err(error), Ok(())) => Err(error),
            (Ok(installed), Err(_)) => Err(Error::message(format!(
                "Native installation was created at {}, but the configuration lock could not be released; inspect it before retrying",
                installed.display()
            ))),
            (Err(error), Err(_)) => Err(Error::message(format!(
                "{error}; the configuration lock could not be released, inspect it before retrying"
            ))),
        }
    }

    pub fn install(args: &[String]) -> Result<()> {
        if matches!(args, [help] if help == "--help" || help == "-h") {
            println!(
                "rdh install-native --prefix ABSOLUTE_DIRECTORY [--config ABSOLUTE_PATH]\nCreates a new native development installation; existing installations are never overwritten.\nWith --config, the existing profile must be stopped and is validated and registered without copying credentials."
            );
            return Ok(());
        }
        let mut prefix = None;
        let mut config = None;
        let mut items = args.iter();
        while let Some(flag) = items.next() {
            let target = match flag.as_str() {
                "--prefix" => &mut prefix,
                "--config" => &mut config,
                _ => return Err(Error::message("Invalid native installation option")),
            };
            if target.is_some() {
                return Err(Error::message("Duplicate native installation option"));
            }
            *target = Some(
                items
                    .next()
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| Error::message(format!("{flag} requires a value")))?,
            );
        }
        let prefix = prefix.ok_or_else(|| {
            Error::message(
                "Use install-native --prefix ABSOLUTE_DIRECTORY [--config ABSOLUTE_PATH]",
            )
        })?;
        let installed = if let Some(config) = config {
            install_with_profile(Path::new(prefix), Path::new(config))?
        } else {
            native_install::install(Path::new(prefix))?
        };
        #[cfg(unix)]
        let launch = installed.join("current/rdh");
        #[cfg(windows)]
        let launch =
            crate::install_lock::version_binary(&installed, &pointer(&installed, "current")?)?;
        println!(
            "Native development installation created: {}\nRun {} to pair or reuse a configuration.",
            installed.display(),
            launch.display()
        );
        if config.is_some() {
            println!(
                "Existing configuration validated and registered without copying credentials. Service startup was not changed."
            );
        }
        Ok(())
    }
    async fn select_release(
        options: &Options,
        current: &str,
        installation_id: &str,
    ) -> Result<Option<(String, Artifact, Location)>> {
        let channel = options.channel.as_deref().unwrap_or("stable");
        // Keep the environment value alive while choosing the default.
        let selected = if options.index.is_some() {
            options.index.clone().unwrap()
        } else if let Ok(value) = std::env::var("RDH_INDEX") {
            value
        } else {
            format!("{DEFAULT_INDEX}/{channel}.json")
        };
        let source = location(&selected)?;
        let raw = read(&source, INDEX_LIMIT).await?;
        let value: serde_json::Value = serde_json::from_slice(&raw)?;
        if value
            .get("channel")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value != channel)
        {
            return Err(Error::message(
                "Native update channel does not match the requested channel",
            ));
        }
        let index = Channel::parse(&value)?;
        let platform = platform()?;
        let decision = index.decide(current, platform, installation_id, channel, options.force)?;
        println!(
            "Current version: {current} ({platform}); target version: {}",
            index.version
        );
        let artifact = match decision {
            Decision::None(reason) | Decision::Blocked(reason) => {
                println!("{reason}");
                return Ok(None);
            }
            Decision::Upgrade(artifact) => artifact,
        };
        let artifact_source = resolve(&source, &artifact.url)?;
        if options.check || options.dry_run {
            println!("Native update is available; no installation changes were made.");
            return Ok(None);
        }
        Ok(Some((index.version, artifact, artifact_source)))
    }

    #[cfg(windows)]
    async fn upgrade_service(options: &Options) -> Result<bool> {
        use crate::service_install::windows::update::Lease;
        let Some(lease) = Lease::acquire_current(options.recover, options.recover)? else {
            return Ok(false);
        };
        if options.list_profiles || options.forget_profile.is_some() || options.recover_lock {
            return Err(Error::message(
                "Service installations use their recorded profile and kernel lock; profile catalog commands and --recover-lock are unavailable",
            ));
        }
        if options.recover {
            lease.recover()?;
            println!(
                "Interrupted service update restored. Start the service manually and verify Hub readiness."
            );
            return Ok(true);
        }
        let Some((version, artifact, source)) = select_release(
            options,
            &lease.record.current.version,
            &lease.record.install_id,
        )
        .await?
        else {
            return Ok(true);
        };
        if !options.no_restart {
            return Err(Error::message(
                "Stop the service and use --no-restart for an offline service update",
            ));
        }
        let manager = lease.exclusive(false)?;
        let bytes = read(&source, MAX_BINARY_BYTES as usize).await?;
        if bytes.len() as u64 != artifact.bytes {
            return Err(Error::message(
                "Native update size does not match the index",
            ));
        }
        manager.stage_and_switch(&version, &bytes, &artifact.sha256)?;
        println!(
            "Service executable switched to {version}. Start the service manually and verify Hub readiness."
        );
        Ok(true)
    }

    pub async fn upgrade(args: &[String]) -> Result<()> {
        if matches!(args, [help] if help == "--help" || help == "-h") {
            print!("{USAGE}");
            return Ok(());
        }
        let options = options(args)?;
        #[cfg(not(target_os = "linux"))]
        if options.recover_service
            || options.probe_profile.is_some()
            || options.stop_if_idle.is_some()
            || options.inspect_service.is_some()
            || options.service_unit.is_some()
            || options.service_config.is_some()
        {
            return Err(Error::message(
                "Coordinated service updates are unavailable on this platform; stop the Agent and use an offline update",
            ));
        }
        #[cfg(windows)]
        if upgrade_service(&options).await? {
            return Ok(());
        }
        let (prefix, executable) = current_installation()?
            .ok_or_else(|| Error::message("This binary is not in a managed native installation"))?;
        if options.recover {
            let manager = ManagerLock::acquire_with_stale_recovery(
                &prefix,
                &executable,
                true,
                options.recover_lock,
            )?;
            native_install::recover(&manager)?;
            println!(
                "Interrupted native update restored. Start the Agent manually after inspection."
            );
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if options.recover_service {
            let accepted =
                crate::service_controller::recover(&prefix, &executable, options.recover_lock)
                    .await?;
            println!("Service update recovery completed: {}", accepted.unit);
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(config) = options.probe_profile.as_deref() {
            let guard = RunningInstall::acquire_for_inspection()?
                .ok_or_else(|| Error::message("Native installation is unavailable"))?;
            let snapshot =
                crate::readiness::probe_profile(&guard, &prefix, Path::new(config)).await?;
            println!("{}", serde_json::to_string(&snapshot)?);
            if !snapshot.ready {
                return Err(Error::message(
                    "The Agent has not received Hub ready on its current connection",
                ));
            }
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let (Some(unit), Some(config)) = (
            options.inspect_service.as_deref(),
            options.service_config.as_deref(),
        ) {
            let guard = RunningInstall::acquire_for_inspection()?
                .ok_or_else(|| Error::message("Native installation is unavailable"))?;
            let identity =
                crate::systemd_identity::inspect(&guard, &prefix, Path::new(config), unit).await?;
            println!("{}", serde_json::to_string(&identity)?);
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(config) = options.stop_if_idle.as_deref() {
            let _controller = crate::install_lock::ControllerLock::acquire(&prefix, &executable)?;
            let guard = RunningInstall::acquire_current()?
                .ok_or_else(|| Error::message("Native installation is unavailable"))?;
            let stopped =
                crate::readiness::stop_if_idle(&guard, &prefix, Path::new(config)).await?;
            println!(
                "{}",
                serde_json::json!({"stopped":true,"pid":stopped.pid,
                "device_id":stopped.device_id,"version":stopped.version,"instance_id":stopped.instance_id})
            );
            return Ok(());
        }
        if options.list_profiles || options.forget_profile.is_some() {
            let manager = ManagerLock::acquire(&prefix, &executable, false)?;
            if options.list_profiles {
                for path in manager.profiles()? {
                    println!("{}", path.display());
                }
            } else if let Some(config) = options.forget_profile.as_deref() {
                manager.forget_profile(Path::new(config))?;
                println!(
                    "Profile removed from the native installation catalog; the configuration was retained."
                );
            }
            return Ok(());
        }
        let guard = RunningInstall::acquire_current()?
            .ok_or_else(|| Error::message("Native installation is unavailable"))?;
        let current = pointer(&prefix, "current")?;
        let Some((version, artifact, artifact_source)) =
            select_release(&options, &current, &install_id(&prefix)?).await?
        else {
            return Ok(());
        };
        #[cfg(target_os = "linux")]
        if let (Some(unit), Some(config)) = (
            options.service_unit.as_deref(),
            options.service_config.as_deref(),
        ) {
            let binary = read(&artifact_source, MAX_BINARY_BYTES as usize).await?;
            if binary.len() as u64 != artifact.bytes {
                return Err(Error::message(
                    "Native update size does not match the index",
                ));
            }
            let accepted = crate::service_controller::upgrade(
                &prefix,
                &executable,
                guard,
                unit,
                Path::new(config),
                crate::service_controller::Candidate {
                    version: &version,
                    binary: &binary,
                    sha256: &artifact.sha256,
                },
            )
            .await?;
            println!(
                "Service {} is running verified native version {}.",
                accepted.unit, accepted.new_current
            );
            return Ok(());
        }
        if !options.no_restart {
            return Err(Error::message(if cfg!(target_os = "linux") {
                "Specify a verified --service-unit and --service-config, or stop the Agent and use --no-restart"
            } else {
                "Stop the Agent and use --no-restart for an offline update"
            }));
        }
        drop(guard);
        #[cfg(not(windows))]
        let manager = ManagerLock::acquire(&prefix, &executable, false)?;
        #[cfg(windows)]
        let manager = ManagerLock::acquire_with_stale_recovery(
            &prefix,
            &executable,
            false,
            options.recover_lock,
        )?;
        native_install::verify_stored(&prefix, &current)?;
        let binary = read(&artifact_source, MAX_BINARY_BYTES as usize).await?;
        if binary.len() as u64 != artifact.bytes {
            return Err(Error::message(
                "Native update size does not match the index",
            ));
        }
        native_install::stage(&manager, &version, &binary, &artifact.sha256)?;
        native_install::verify_stored(&prefix, &version)?;
        let _profile_locks = native_install::validate_profiles(&manager, &version)?;
        native_install::switch(&manager, &version)?;
        #[cfg(windows)]
        println!(
            "Selected executable: {}",
            crate::install_lock::version_binary(&prefix, &version)?.display()
        );
        println!(
            "Switched to {}; start the Agent manually and verify Hub readiness.",
            version
        );
        Ok(())
    }
    pub fn rollback(args: &[String]) -> Result<()> {
        if matches!(args, [help] if help == "--help" || help == "-h") {
            print!("{USAGE}");
            return Ok(());
        }
        if !matches!(args, [flag] if flag == "--no-restart") {
            return Err(Error::message(
                "Use rollback --no-restart after stopping the Agent",
            ));
        }
        #[cfg(windows)]
        if let Some(manager) =
            crate::service_install::windows::update::Lease::acquire_current(true, false)?
        {
            manager.rollback()?;
            println!(
                "Service executable rolled back. Start the service manually and verify Hub readiness."
            );
            return Ok(());
        }
        let (prefix, executable) = current_installation()?
            .ok_or_else(|| Error::message("This binary is not in a managed native installation"))?;
        let manager = ManagerLock::acquire(&prefix, &executable, false)?;
        let current = pointer(&prefix, "current")?;
        let previous = pointer(&prefix, "previous")?;
        native_install::verify_stored(&prefix, &current)?;
        native_install::verify_stored(&prefix, &previous)?;
        #[cfg(windows)]
        let _profiles = ProfileLocks::acquire(&manager.profiles()?)?;
        native_install::switch(&manager, &previous)?;
        #[cfg(windows)]
        println!(
            "Selected executable: {}",
            crate::install_lock::version_binary(&prefix, &previous)?.display()
        );
        println!("Rolled back to {previous}; start the Agent manually and verify Hub readiness.");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn local_profile_commands_cannot_be_combined_with_an_update_or_recovery() {
            let args = |items: &[&str]| {
                items
                    .iter()
                    .map(|item| (*item).to_owned())
                    .collect::<Vec<_>>()
            };
            for local in ["--probe-profile", "--stop-if-idle"] {
                assert!(options(&args(&[local, "/tmp/device.json"])).is_ok());
                for conflicting in ["--no-restart", "--check", "--recover", "--list-profiles"] {
                    assert!(options(&args(&[local, "/tmp/device.json", conflicting])).is_err());
                }
            }
            assert!(
                options(&args(&[
                    "--probe-profile",
                    "/tmp/device.json",
                    "--stop-if-idle",
                    "/tmp/device.json"
                ]))
                .is_err()
            );
            assert!(
                options(&args(&[
                    "--service-unit",
                    "rdh.service",
                    "--service-config",
                    "/tmp/device.json",
                    "--index",
                    "/tmp/channel.json"
                ]))
                .is_ok()
            );
            assert!(options(&args(&["--service-unit", "rdh.service"])).is_err());
            assert!(
                options(&args(&[
                    "--service-unit",
                    "rdh.service",
                    "--service-config",
                    "/tmp/device.json",
                    "--no-restart"
                ]))
                .is_err()
            );
            assert!(options(&args(&["--recover-service", "--recover-lock"])).is_ok());
            assert!(
                options(&args(&[
                    "--recover-service",
                    "--index",
                    "/tmp/channel.json"
                ]))
                .is_err()
            );
            assert!(
                options(&args(&[
                    "--inspect-service",
                    "rdh.service",
                    "--service-config",
                    "/tmp/device.json"
                ]))
                .is_ok()
            );
            assert!(options(&args(&["--inspect-service", "rdh.service"])).is_err());
            assert!(options(&args(&["--service-config", "/tmp/device.json"])).is_err());
            assert!(
                options(&args(&[
                    "--inspect-service",
                    "rdh.service",
                    "--service-config",
                    "/tmp/device.json",
                    "--probe-profile",
                    "/tmp/device.json"
                ]))
                .is_err()
            );
        }
        #[test]
        fn remote_indexes_cannot_select_local_files_or_downgrade_to_http() {
            let base = Location::Remote(Url::parse("https://example.test/channel.json").unwrap());
            assert!(resolve(&base, "file:///tmp/rdh").is_err());
            assert!(resolve(&base, "http://127.0.0.1/rdh").is_err());
            assert!(
                matches!(resolve(&base, "rdh").unwrap(), Location::Remote(url) if url.as_str() == "https://example.test/rdh")
            );
        }
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
))]
pub use native::{install, rollback, upgrade};
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
pub fn install(_: &[String]) -> crate::error::Result<()> {
    Err(crate::error::Error::message(
        "Native installation is unsupported on this platform",
    ))
}
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
pub async fn upgrade(_: &[String]) -> crate::error::Result<()> {
    Err(crate::error::Error::message(
        "Native updates are unsupported on this platform",
    ))
}
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
pub fn rollback(_: &[String]) -> crate::error::Result<()> {
    Err(crate::error::Error::message(
        "Native rollback is unsupported on this platform",
    ))
}
