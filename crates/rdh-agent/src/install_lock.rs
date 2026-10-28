//! Native installation exclusion and crash markers.
//! A live Agent holds a shared lock; an offline updater must hold it exclusively.

#[cfg(all(windows, feature = "experimental-windows-install"))]
#[path = "install_lock_windows.rs"]
pub(crate) mod windows;

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub use windows::{ManagerLock, PENDING, RunningInstall, current_installation, pointer};

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) use windows::{pending, record_profile, version_binary};

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
mod posix {
    use std::fs::{self, File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Component, Path, PathBuf};

    use serde_json::{Value, json};

    use crate::error::{Error, Result};
    use crate::{random, storage, update};

    const MARKER: &str = ".native-install.json";
    const LOCK: &str = ".manager.lock";
    const RUNNING: &str = ".running";
    const PROFILES: &str = ".profiles";
    const PROFILES_LOCK: &str = ".profiles.lock";
    const LAUNCH_LOCK: &str = ".launch.lock";
    #[cfg(target_os = "linux")]
    const CONTROLLER_LOCK: &str = ".service-controller.lock";
    const MAX_PROFILES: usize = 256;
    pub const PENDING: &str = ".update-pending.json";

    fn private_directory(path: &Path) -> Result<()> {
        let metadata = storage::private_metadata(path, true)?;
        if metadata.mode() & 0o077 != 0 {
            return Err(Error::message(
                "Native installation directory is not private",
            ));
        }
        Ok(())
    }
    fn marker_valid(prefix: &Path) -> Result<()> {
        private_directory(prefix)?;
        let bytes = storage::read_private(&prefix.join(MARKER))?;
        if bytes.len() > 4096 {
            return Err(Error::message("Invalid native installation marker"));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        if value["schema"].as_u64() != Some(1) || value["kind"].as_str() != Some("native-agent") {
            return Err(Error::message("Invalid native installation marker"));
        }
        Ok(())
    }
    pub fn pointer(prefix: &Path, name: &str) -> Result<String> {
        if name != "current" && name != "previous" {
            return Err(Error::message("Invalid installation pointer"));
        }
        let path = fs::read_link(prefix.join(name))?;
        let mut parts = path.components();
        if parts.next() != Some(Component::Normal("versions".as_ref())) {
            return Err(Error::message("Invalid installation pointer"));
        }
        let version = match parts.next() {
            Some(Component::Normal(value)) => value.to_str().unwrap_or(""),
            _ => "",
        };
        if parts.next().is_some() || !update::valid_version(version) {
            return Err(Error::message("Invalid installation pointer"));
        }
        Ok(version.into())
    }
    pub(crate) fn version_binary(prefix: &Path, version: &str) -> Result<PathBuf> {
        if !update::valid_version(version) {
            return Err(Error::message("Invalid native version"));
        }
        let versions = prefix.join("versions");
        private_directory(&versions)?;
        let directory = versions.join(version);
        private_directory(&directory)?;
        let candidate = directory.join("rdh");
        let named = storage::private_metadata(&candidate, false)?;
        if named.nlink() != 1 || named.mode() & 0o100 == 0 {
            return Err(Error::message("Invalid native version binary"));
        }
        Ok(candidate)
    }
    fn verify_version(prefix: &Path, version: &str, executable: &Path) -> Result<()> {
        let candidate = version_binary(prefix, version)?;
        let named = fs::metadata(&candidate)?;
        let actual = fs::metadata(executable)?;
        if named.dev() != actual.dev() || named.ino() != actual.ino() {
            return Err(Error::message(
                "This executable does not match the selected native version",
            ));
        }
        Ok(())
    }
    fn verify_current(prefix: &Path, executable: &Path) -> Result<()> {
        verify_version(prefix, &pointer(prefix, "current")?, executable)
    }
    pub(crate) fn pending(prefix: &Path) -> Result<Value> {
        let bytes = storage::read_private(&prefix.join(PENDING))?;
        if bytes.len() > 8192 {
            return Err(Error::message("Invalid native update record"));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        let valid = |key: &str| value[key].as_str().is_some_and(update::valid_version);
        if value["schema"].as_u64() != Some(1)
            || !value
                .as_object()
                .is_some_and(|object| object.contains_key("old_previous"))
            || !valid("old_current")
            || !valid("new_current")
            || (!value["old_previous"].is_null() && !valid("old_previous"))
        {
            return Err(Error::message("Invalid native update record"));
        }
        Ok(value)
    }
    struct FileLock(File);
    impl Drop for FileLock {
        fn drop(&mut self) {
            // A fork in another thread can temporarily inherit this open file
            // description before close-on-exec. Explicit unlock avoids leaving
            // our lock held by such a child after the owner has finished.
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
    fn lock_file(prefix: &Path, name: &str, exclusive: bool) -> Result<FileLock> {
        marker_valid(prefix)?;
        let path = prefix.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let held = file.metadata()?;
        let named = storage::private_metadata(&path, false)?;
        if !held.is_file()
            || held.nlink() != 1
            || held.dev() != named.dev()
            || held.ino() != named.ino()
        {
            return Err(Error::message("Native installation lock changed"));
        }
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        } | libc::LOCK_NB;
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            return Err(Error::message(if name == PROFILES_LOCK {
                "A native profile registration is in progress; retry"
            } else if exclusive {
                "A native Agent or installation manager is active; finish its tasks and stop it before updating"
            } else {
                "A native installation update is in progress"
            }));
        }
        let file = FileLock(file);
        let current = storage::private_metadata(&path, false)?;
        if held.dev() != current.dev() || held.ino() != current.ino() {
            return Err(Error::message("Native installation lock changed"));
        }
        Ok(file)
    }
    fn open_lock(prefix: &Path, exclusive: bool) -> Result<FileLock> {
        lock_file(prefix, LOCK, exclusive)
    }
    fn ensure_no_switch_pending(prefix: &Path) -> Result<()> {
        match fs::symlink_metadata(prefix.join(PENDING)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(Error::message(
                "A native update was interrupted; recover it before starting the Agent",
            )),
            Err(error) => Err(error.into()),
        }
    }
    fn ensure_no_service_pending(prefix: &Path) -> Result<()> {
        #[cfg(target_os = "linux")]
        let exists = crate::service_update::read(prefix)?.is_some();
        #[cfg(target_os = "macos")]
        let exists = match fs::symlink_metadata(prefix.join(".service-update.json")) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if exists {
            return Err(Error::message(
                "A service update is pending; inspect and recover it before managing the installation",
            ));
        }
        Ok(())
    }
    fn ensure_no_pending(prefix: &Path) -> Result<()> {
        ensure_no_switch_pending(prefix)?;
        ensure_no_service_pending(prefix)
    }

    /// A service controller holds this only across decisions that must not
    /// race an Agent between reading update state and publishing its marker.
    #[cfg(target_os = "linux")]
    pub struct LaunchBarrier {
        prefix: PathBuf,
        _lock: FileLock,
    }
    #[cfg(target_os = "linux")]
    impl LaunchBarrier {
        pub fn acquire(prefix: &Path) -> Result<Self> {
            Ok(Self {
                prefix: prefix.to_path_buf(),
                _lock: lock_file(prefix, LAUNCH_LOCK, true)?,
            })
        }
        pub fn prefix(&self) -> &Path {
            &self.prefix
        }
    }

    /// Serializes an entire service update, including the time the Agent must
    /// continue to hold the installation's shared lock.
    #[cfg(target_os = "linux")]
    pub struct ControllerLock {
        prefix: PathBuf,
        _lock: FileLock,
    }
    #[cfg(target_os = "linux")]
    impl ControllerLock {
        pub fn acquire(prefix: &Path, executable: &Path) -> Result<Self> {
            let lock = lock_file(prefix, CONTROLLER_LOCK, true)?;
            if let Some(intent) = crate::service_update::read(prefix)? {
                if verify_version(prefix, &intent.old_current, executable).is_err()
                    && verify_version(prefix, &intent.new_current, executable).is_err()
                {
                    return Err(Error::message(
                        "The controller executable is not part of the service update",
                    ));
                }
            } else {
                verify_current(prefix, executable)?;
            }
            Ok(Self {
                prefix: prefix.to_path_buf(),
                _lock: lock,
            })
        }
        pub fn prefix(&self) -> &Path {
            &self.prefix
        }
    }
    fn private_subdirectory(prefix: &Path, name: &str) -> Result<PathBuf> {
        let path = prefix.join(name);
        match storage::create_private_dir(&path) {
            Ok(_) => {
                storage::sync_directory(&File::open(prefix)?)?;
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                private_directory(&path)?
            }
            Err(error) => return Err(error),
        }
        Ok(path)
    }
    fn running_dir(prefix: &Path) -> Result<PathBuf> {
        private_subdirectory(prefix, RUNNING)
    }
    fn profile_from_file(path: &Path) -> Result<PathBuf> {
        let bytes = storage::read_private(path)?;
        if bytes.len() > 8192 {
            return Err(Error::message("Invalid native profile record"));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        let config = value["config"]
            .as_str()
            .filter(|path| Path::new(path).is_absolute())
            .ok_or_else(|| Error::message("Invalid native profile record"))?;
        let expected = format!("{}.json", crate::config::sha256_hex(config.as_bytes()));
        if value["schema"].as_u64() != Some(1)
            || path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
        {
            return Err(Error::message("Invalid native profile record"));
        }
        Ok(PathBuf::from(config))
    }
    fn registered_profiles(prefix: &Path) -> Result<Vec<PathBuf>> {
        let directory = private_subdirectory(prefix, PROFILES)?;
        let mut result = Vec::new();
        for entry in fs::read_dir(directory)? {
            if result.len() >= MAX_PROFILES {
                return Err(Error::message("Native installation has too many profiles"));
            }
            result.push(profile_from_file(&entry?.path())?);
        }
        result.sort();
        Ok(result)
    }

    /// The caller holds either the live installation lock or an unpublished
    /// private staging directory. Only the canonical path is recorded.
    pub(crate) fn record_profile(prefix: &Path, config: &Path) -> Result<()> {
        let config = config.canonicalize()?;
        let text = config
            .to_str()
            .ok_or_else(|| Error::message("Config path must be Unicode"))?;
        let _guard = lock_file(prefix, PROFILES_LOCK, true)?;
        let directory = private_subdirectory(prefix, PROFILES)?;
        let path = directory.join(format!(
            "{}.json",
            crate::config::sha256_hex(text.as_bytes())
        ));
        let value = json!({"schema":1,"config":text});
        let data = serde_json::to_vec(&value)?;
        if data.len() > 8192 {
            return Err(Error::message("Native profile record is too large"));
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return if profile_from_file(&path)? == config {
                    Ok(())
                } else {
                    Err(Error::message("Native profile record changed"))
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if registered_profiles(prefix)?.len() >= MAX_PROFILES {
            return Err(Error::message("Native installation has too many profiles"));
        }
        match storage::write_private_new(&path, &data) {
            Ok(()) => Ok(()),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if profile_from_file(&path)? == config {
                    Ok(())
                } else {
                    Err(Error::message("Native profile record changed"))
                }
            }
            Err(error) => Err(error),
        }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RunningRecord {
        pub pid: u32,
        pub config: PathBuf,
        pub nonce: String,
    }

    fn parse_marker(path: &Path) -> Result<RunningRecord> {
        let bytes = storage::read_private(path)?;
        if bytes.len() > 8192 {
            return Err(Error::message("Invalid native running marker"));
        }
        let value: Value = serde_json::from_slice(&bytes)?;
        let pid = value["pid"]
            .as_u64()
            .filter(|value| *value > 0 && *value <= i32::MAX as u64)
            .ok_or_else(|| Error::message("Invalid native running marker"))?
            as u32;
        let config = value["config"]
            .as_str()
            .filter(|path| Path::new(path).is_absolute())
            .ok_or_else(|| Error::message("Invalid native running marker"))?;
        let nonce = value["nonce"]
            .as_str()
            .filter(|value| {
                value.len() == 32
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
            .ok_or_else(|| Error::message("Invalid native running marker"))?;
        if value["schema"].as_u64() != Some(1)
            || path.file_name().and_then(|name| name.to_str())
                != Some(&format!("{pid}-{nonce}.json"))
        {
            return Err(Error::message("Invalid native running marker"));
        }
        Ok(RunningRecord {
            pid,
            config: config.into(),
            nonce: nonce.into(),
        })
    }
    fn pid_is_dead(pid: u32) -> Result<bool> {
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(true)
        } else if error.raw_os_error() == Some(libc::EPERM) {
            Ok(false)
        } else {
            Err(error.into())
        }
    }
    fn remove_same(path: &Path, before: &std::fs::Metadata) -> Result<()> {
        let now = storage::private_metadata(path, false)?;
        if now.dev() != before.dev() || now.ino() != before.ino() || now.nlink() != 1 {
            return Err(Error::message("Native state record changed"));
        }
        fs::remove_file(path)?;
        storage::sync_directory(&File::open(
            path.parent()
                .ok_or_else(|| Error::message("Invalid marker parent"))?,
        )?)?;
        Ok(())
    }
    fn recover_for_config(directory: &Path, config: &Path) -> Result<()> {
        let config = config
            .to_str()
            .ok_or_else(|| Error::message("Config path is not Unicode"))?;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let before = storage::private_metadata(&path, false)?;
            let record = parse_marker(&path)?;
            if record.config == Path::new(config) {
                if !pid_is_dead(record.pid)? {
                    return Err(Error::message(
                        "A previous native Agent process still exists",
                    ));
                }
                remove_same(&path, &before)?;
            }
        }
        Ok(())
    }

    pub struct RunningInstall {
        prefix: PathBuf,
        _lock: FileLock,
        launch_lock: Option<FileLock>,
        #[cfg(target_os = "linux")]
        launch: Option<crate::service_update::Launch>,
        marker: Option<(PathBuf, std::fs::Metadata)>,
    }
    impl RunningInstall {
        pub fn prefix(&self) -> &Path {
            &self.prefix
        }
        pub fn acquire_current() -> Result<Option<Self>> {
            let Some((prefix, executable)) = current_installation()? else {
                return Ok(None);
            };
            Self::acquire(&prefix, &executable).map(Some)
        }
        fn acquire(prefix: &Path, executable: &Path) -> Result<Self> {
            let lock = open_lock(prefix, false)?;
            ensure_no_pending(prefix)?;
            verify_current(prefix, executable)?;
            Ok(Self {
                prefix: prefix.into(),
                _lock: lock,
                launch_lock: None,
                #[cfg(target_os = "linux")]
                launch: None,
                marker: None,
            })
        }
        /// A launch pins its update decision until its running marker and
        /// control socket have been published. This closes the check/start
        /// race when a controller later commits or clears an update record.
        pub fn acquire_launch_current() -> Result<Option<Self>> {
            let Some((prefix, executable)) = current_installation()? else {
                return Ok(None);
            };
            let launch_lock = lock_file(&prefix, LAUNCH_LOCK, false)?;
            let lock = open_lock(&prefix, false)?;
            ensure_no_switch_pending(&prefix)?;
            let current = pointer(&prefix, "current")?;
            verify_version(&prefix, &current, &executable)?;
            #[cfg(target_os = "linux")]
            let launch = crate::service_update::launch(&prefix, &current)?;
            #[cfg(target_os = "macos")]
            ensure_no_service_pending(&prefix)?;
            Ok(Some(Self {
                prefix,
                _lock: lock,
                launch_lock: Some(launch_lock),
                #[cfg(target_os = "linux")]
                launch,
                marker: None,
            }))
        }
        /// Readiness inspection is allowed while an update is pending, but
        /// still requires a valid record and the current executable.
        pub fn acquire_for_inspection() -> Result<Option<Self>> {
            let Some((prefix, executable)) = current_installation()? else {
                return Ok(None);
            };
            Self::inspect_at(&prefix, &executable).map(Some)
        }
        pub fn inspect_at(prefix: &Path, executable: &Path) -> Result<Self> {
            let lock = open_lock(prefix, false)?;
            ensure_no_switch_pending(prefix)?;
            #[cfg(target_os = "linux")]
            crate::service_update::read(prefix)?;
            #[cfg(target_os = "macos")]
            ensure_no_service_pending(prefix)?;
            verify_current(prefix, executable)?;
            Ok(Self {
                prefix: prefix.to_path_buf(),
                _lock: lock,
                launch_lock: None,
                #[cfg(target_os = "linux")]
                launch: None,
                marker: None,
            })
        }
        pub fn check_launch_profile(&self, config: &Path, pair_only: bool) -> Result<()> {
            #[cfg(target_os = "linux")]
            if let Some(launch) = &self.launch {
                launch.check_profile(config, pair_only)?;
            }
            #[cfg(target_os = "macos")]
            let _ = (config, pair_only);
            Ok(())
        }
        #[cfg(target_os = "linux")]
        pub fn trial_launch(&self) -> Option<&crate::service_update::Launch> {
            self.launch.as_ref().filter(|launch| launch.trial)
        }
        pub fn finish_launch(&mut self) {
            self.launch_lock.take();
        }
        /// The marker identifies this particular Agent process, not merely the
        /// installation or the profile. It is created before work is accepted.
        pub fn running_record(&self) -> Result<RunningRecord> {
            let (path, original) = self
                .marker
                .as_ref()
                .ok_or_else(|| Error::message("Native Agent is not marked running"))?;
            let current = storage::private_metadata(path, false)?;
            if current.dev() != original.dev()
                || current.ino() != original.ino()
                || current.nlink() != 1
            {
                return Err(Error::message("Native running marker changed"));
            }
            let record = parse_marker(path)?;
            if record.pid != std::process::id() {
                return Err(Error::message(
                    "Native running marker belongs to another process",
                ));
            }
            Ok(record)
        }
        /// Enumerate under the caller's shared installation lock. A marker
        /// alone is never proof that its process or Hub connection is alive.
        pub fn running_records(&self) -> Result<Vec<RunningRecord>> {
            let mut records = Vec::new();
            for entry in fs::read_dir(running_dir(&self.prefix)?)? {
                if records.len() >= MAX_PROFILES {
                    return Err(Error::message(
                        "Native installation has too many running markers",
                    ));
                }
                records.push(parse_marker(&entry?.path())?);
            }
            Ok(records)
        }
        /// Persist the configuration path so future versions can validate it
        /// before switching. The record contains no device token.
        pub fn register_profile(&self, config: &Path) -> Result<()> {
            record_profile(&self.prefix, config)
        }
        /// Call after acquiring/recovering the profile lock, before accepting work.
        pub fn mark_running(&mut self, config: &Path, recover: bool) -> Result<()> {
            let directory = running_dir(&self.prefix)?;
            if recover {
                recover_for_config(&directory, config)?;
            }
            let config = config
                .to_str()
                .ok_or_else(|| Error::message("Config path is not Unicode"))?;
            for entry in fs::read_dir(&directory)? {
                let record = parse_marker(&entry?.path())?;
                if record.config == Path::new(config) {
                    return Err(Error::message(
                        "A native running marker remains for this profile; inspect and recover the previous Agent before starting",
                    ));
                }
            }
            let nonce = random::hex_token::<16>()?;
            let path = directory.join(format!("{}-{nonce}.json", std::process::id()));
            let value = json!({"schema":1,"pid":std::process::id(),"config":config,"nonce":nonce});
            storage::write_private_new(&path, &serde_json::to_vec(&value)?)?;
            let metadata = storage::private_metadata(&path, false)?;
            self.marker = Some((path, metadata));
            Ok(())
        }
        /// Only remove the marker after all effectful work and cleanup finished.
        pub fn mark_clean(&mut self) -> Result<()> {
            if let Some((path, metadata)) = self.marker.take() {
                remove_same(&path, &metadata)?;
            }
            Ok(())
        }
    }

    pub fn current_installation() -> Result<Option<(PathBuf, PathBuf)>> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let version = match executable.parent() {
            Some(path) => path,
            None => return Ok(None),
        };
        let versions = match version.parent() {
            Some(path) if path.file_name().is_some_and(|name| name == "versions") => path,
            _ => return Ok(None),
        };
        let prefix = versions
            .parent()
            .ok_or_else(|| Error::message("Invalid installation prefix"))?;
        match fs::symlink_metadata(prefix.join(MARKER)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        Ok(Some((prefix.to_path_buf(), executable)))
    }

    pub struct ManagerLock {
        pub prefix: PathBuf,
        _lock: FileLock,
        #[cfg(target_os = "linux")]
        service_transaction: Option<String>,
    }
    impl ManagerLock {
        pub fn profiles(&self) -> Result<Vec<PathBuf>> {
            registered_profiles(&self.prefix)
        }
        /// Forget only the catalog entry; never delete the configuration.
        pub fn forget_profile(&self, config: &Path) -> Result<()> {
            let text = config
                .to_str()
                .filter(|_| config.is_absolute())
                .ok_or_else(|| Error::message("Profile path must be absolute Unicode"))?;
            let path = private_subdirectory(&self.prefix, PROFILES)?.join(format!(
                "{}.json",
                crate::config::sha256_hex(text.as_bytes())
            ));
            let before = storage::private_metadata(&path, false)?;
            if profile_from_file(&path)? != config {
                return Err(Error::message("Native profile record changed"));
            }
            // If the parent still exists, prevent a cooperating legacy Agent
            // from using the configuration while the entry is removed.
            let _profile_lock = if config.parent().is_some_and(Path::is_dir) {
                Some(crate::profile_lock::ProfileLocks::acquire(&[
                    config.to_path_buf()
                ])?)
            } else {
                None
            };
            remove_same(&path, &before)
        }
        pub fn acquire(prefix: &Path, executable: &Path, allow_pending: bool) -> Result<Self> {
            Self::acquire_with_stale_recovery(prefix, executable, allow_pending, false)
        }
        pub fn acquire_with_stale_recovery(
            prefix: &Path,
            executable: &Path,
            allow_pending: bool,
            recover_stale: bool,
        ) -> Result<Self> {
            let lock = open_lock(prefix, true)?;
            ensure_no_service_pending(prefix)?;
            if allow_pending {
                let pending = pending(prefix)?;
                let old = pending["old_current"].as_str().unwrap();
                let new = pending["new_current"].as_str().unwrap();
                if verify_version(prefix, old, executable).is_err()
                    && verify_version(prefix, new, executable).is_err()
                {
                    return Err(Error::message(
                        "Recovery executable is not part of the pending update",
                    ));
                }
            } else {
                ensure_no_pending(prefix)?;
                verify_current(prefix, executable)?;
            }
            let directory = running_dir(prefix)?;
            if recover_stale {
                for entry in fs::read_dir(&directory)? {
                    let path = entry?.path();
                    let metadata = storage::private_metadata(&path, false)?;
                    let record = parse_marker(&path)?;
                    if !pid_is_dead(record.pid)? {
                        return Err(Error::message(
                            "A previous native Agent process still exists",
                        ));
                    }
                    crate::profile_lock::ProfileLocks::recover(&[record.config])?;
                    remove_same(&path, &metadata)?;
                }
            }
            if fs::read_dir(directory)?.next().transpose()?.is_some() {
                return Err(Error::message(
                    "Native Agent running or unclean-shutdown markers remain; inspect and recover their profile locks before updating",
                ));
            }
            Ok(Self {
                prefix: prefix.into(),
                _lock: lock,
                #[cfg(target_os = "linux")]
                service_transaction: None,
            })
        }

        /// Acquire exclusive installation ownership for a serialized service
        /// controller. A switch journal, when present, must belong to this
        /// exact service transaction before it can be recovered.
        #[cfg(target_os = "linux")]
        pub fn for_service(
            controller: &ControllerLock,
            executable: &Path,
            allow_switch_pending: bool,
            recover_stale: bool,
        ) -> Result<Self> {
            let prefix = controller.prefix();
            let lock = open_lock(prefix, true)?;
            let intent = crate::service_update::read(prefix)?
                .ok_or_else(|| Error::message("No service update is pending"))?;
            let current = pointer(prefix, "current")?;
            if current != intent.old_current && current != intent.new_current {
                return Err(Error::message(
                    "The current version is unrelated to the service update",
                ));
            }
            if verify_version(prefix, &intent.old_current, executable).is_err()
                && verify_version(prefix, &intent.new_current, executable).is_err()
            {
                return Err(Error::message(
                    "The controller executable is not part of the service update",
                ));
            }
            match fs::symlink_metadata(prefix.join(PENDING)) {
                Ok(_) if allow_switch_pending => {
                    let pending = pending(prefix)?;
                    if pending["old_current"].as_str() != Some(intent.old_current.as_str())
                        || pending["new_current"].as_str() != Some(intent.new_current.as_str())
                        || pending["old_previous"].as_str() != intent.old_previous.as_deref()
                    {
                        return Err(Error::message(
                            "The interrupted switch does not belong to this service update",
                        ));
                    }
                }
                Ok(_) => return Err(Error::message("A native switch requires recovery first")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let directory = running_dir(prefix)?;
            if recover_stale {
                for entry in fs::read_dir(&directory)? {
                    let path = entry?.path();
                    let metadata = storage::private_metadata(&path, false)?;
                    let record = parse_marker(&path)?;
                    if !pid_is_dead(record.pid)? {
                        return Err(Error::message(
                            "A previous native Agent process still exists",
                        ));
                    }
                    crate::profile_lock::ProfileLocks::recover(&[record.config])?;
                    remove_same(&path, &metadata)?;
                }
            }
            if fs::read_dir(directory)?.next().transpose()?.is_some() {
                return Err(Error::message(
                    "Native Agent running or unclean-shutdown markers remain; inspect before recovering the service update",
                ));
            }
            Ok(Self {
                prefix: prefix.into(),
                _lock: lock,
                service_transaction: Some(intent.transaction_id),
            })
        }

        #[cfg(target_os = "linux")]
        pub fn owns_service(&self, transaction_id: &str) -> bool {
            self.service_transaction.as_deref() == Some(transaction_id)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        struct Fixture {
            prefix: PathBuf,
            executable: PathBuf,
            config: PathBuf,
        }
        impl Fixture {
            fn new() -> Self {
                let prefix = std::env::temp_dir().join(format!(
                    "rdh-install-lock-{}",
                    random::hex_token::<8>().unwrap()
                ));
                fs::create_dir(&prefix).unwrap();
                fs::set_permissions(&prefix, fs::Permissions::from_mode(0o700)).unwrap();
                let version = prefix.join("versions/1.0.0");
                fs::create_dir_all(&version).unwrap();
                fs::set_permissions(prefix.join("versions"), fs::Permissions::from_mode(0o700))
                    .unwrap();
                fs::set_permissions(&version, fs::Permissions::from_mode(0o700)).unwrap();
                let executable = version.join("rdh");
                fs::write(&executable, b"fixture").unwrap();
                fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
                symlink("versions/1.0.0", prefix.join("current")).unwrap();
                storage::write_private_new(
                    &prefix.join(MARKER),
                    br#"{"schema":1,"kind":"native-agent"}"#,
                )
                .unwrap();
                let config = prefix.join("device.json");
                Self {
                    prefix,
                    executable,
                    config,
                }
            }
            fn stale(&self, pid: u32) -> PathBuf {
                let directory = running_dir(&self.prefix).unwrap();
                let nonce = "a".repeat(32);
                let path = directory.join(format!("{pid}-{nonce}.json"));
                let data = json!({"schema":1,"pid":pid,"config":self.config,"nonce":nonce});
                storage::write_private_new(&path, &serde_json::to_vec(&data).unwrap()).unwrap();
                path
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.prefix);
            }
        }

        #[test]
        fn explicit_unlock_releases_a_description_inherited_across_fork() {
            let fixture = Fixture::new();
            for exclusive in [false, true] {
                let held = open_lock(&fixture.prefix, exclusive).unwrap();
                use std::os::fd::IntoRawFd;
                let (left, right) = std::os::unix::net::UnixStream::pair().unwrap();
                let pipe = [left.into_raw_fd(), right.into_raw_fd()];
                // The child only calls async-signal-safe syscalls before _exit.
                let child = unsafe { libc::fork() };
                if child == 0 {
                    unsafe {
                        libc::close(pipe[1]);
                        let mut byte = 0u8;
                        libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                        libc::_exit(0);
                    }
                }
                assert!(child > 0);
                unsafe { libc::close(pipe[0]) };
                drop(held);
                let acquired = open_lock(&fixture.prefix, !exclusive);
                unsafe {
                    libc::write(pipe[1], b"x".as_ptr().cast(), 1);
                    libc::close(pipe[1]);
                    libc::waitpid(child, std::ptr::null_mut(), 0);
                }
                assert!(acquired.is_ok());
            }
        }

        #[test]
        fn exclusive_update_waits_for_real_agent_and_its_cleanup_marker() {
            let fixture = Fixture::new();
            let mut agent = RunningInstall::acquire(&fixture.prefix, &fixture.executable).unwrap();
            agent.mark_running(&fixture.config, false).unwrap();
            assert!(ManagerLock::acquire(&fixture.prefix, &fixture.executable, false).is_err());
            agent.mark_clean().unwrap();
            drop(agent);
            assert!(ManagerLock::acquire(&fixture.prefix, &fixture.executable, false).is_ok());
        }

        #[test]
        fn stale_or_live_markers_require_explicit_dead_owner_recovery() {
            let fixture = Fixture::new();
            let dead = fixture.stale(i32::MAX as u32);
            assert!(ManagerLock::acquire(&fixture.prefix, &fixture.executable, false).is_err());
            let mut agent = RunningInstall::acquire(&fixture.prefix, &fixture.executable).unwrap();
            assert!(agent.mark_running(&fixture.config, false).is_err());
            agent.mark_running(&fixture.config, true).unwrap();
            assert!(!dead.exists());
            agent.mark_clean().unwrap();
            drop(agent);
            let alive = fixture.stale(std::process::id());
            let mut agent = RunningInstall::acquire(&fixture.prefix, &fixture.executable).unwrap();
            assert!(agent.mark_running(&fixture.config, true).is_err());
            assert!(alive.exists());
        }

        #[test]
        fn interrupted_updates_and_stale_executables_cannot_start() {
            let fixture = Fixture::new();
            storage::write_private_new(
                &fixture.prefix.join(PENDING),
                br#"{"schema":1,"old_current":"1.0.0","new_current":"1.0.0","old_previous":null}"#,
            )
            .unwrap();
            assert!(RunningInstall::acquire(&fixture.prefix, &fixture.executable).is_err());
            assert!(ManagerLock::acquire(&fixture.prefix, &fixture.executable, false).is_err());
            assert!(ManagerLock::acquire(&fixture.prefix, &fixture.executable, true).is_ok());
            fs::remove_file(fixture.prefix.join(PENDING)).unwrap();
            fs::remove_file(fixture.prefix.join("current")).unwrap();
            fs::create_dir_all(fixture.prefix.join("versions/2.0.0")).unwrap();
            fs::set_permissions(
                fixture.prefix.join("versions/2.0.0"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::write(fixture.prefix.join("versions/2.0.0/rdh"), b"new").unwrap();
            fs::set_permissions(
                fixture.prefix.join("versions/2.0.0/rdh"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            symlink("versions/2.0.0", fixture.prefix.join("current")).unwrap();
            assert!(RunningInstall::acquire(&fixture.prefix, &fixture.executable).is_err());
        }
    }
}

#[cfg(target_os = "linux")]
pub use posix::{ControllerLock, LaunchBarrier};
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub use posix::{
    ManagerLock, PENDING, RunningInstall, RunningRecord, current_installation, pointer,
};
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
))]
pub(crate) use posix::{pending, record_profile, version_binary};

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
pub struct RunningInstall;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
)))]
impl RunningInstall {
    pub fn acquire_current() -> crate::error::Result<Option<Self>> {
        Ok(None)
    }
    pub fn acquire_launch_current() -> crate::error::Result<Option<Self>> {
        Ok(None)
    }
    pub fn check_launch_profile(&self, _: &std::path::Path, _: bool) -> crate::error::Result<()> {
        Ok(())
    }
    pub fn finish_launch(&mut self) {}
    pub fn mark_running(&mut self, _: &std::path::Path, _: bool) -> crate::error::Result<()> {
        Ok(())
    }
    pub fn mark_clean(&mut self) -> crate::error::Result<()> {
        Ok(())
    }
    pub fn register_profile(&self, _: &std::path::Path) -> crate::error::Result<()> {
        Ok(())
    }
}
