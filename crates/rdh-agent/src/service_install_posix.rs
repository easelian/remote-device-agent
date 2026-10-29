//! Root-owned programs and service-user-owned state remain separate. Installation
//! creates a stopped registration; version changes require an offline service.
use crate::{
    cli::LaunchArgs,
    error::{Error, Result},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

#[path = "service_migration_posix.rs"]
mod migration;
#[path = "service_profile_posix.rs"]
mod profile;
#[path = "service_storage_posix.rs"]
mod store;
use profile::{Account, ProfileLease};

const RECORD: &str = ".service-install.json";
const LOCK: &str = ".service-manager.lock";
const PENDING: &str = ".service-update-pending.json";
const INSTALL_PENDING: &str = ".service-install-pending";
const RECORD_LIMIT: u64 = 65536;

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().filter(|value| path.is_absolute() && !value.chars().any(char::is_control)
        && !path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)))
        .ok_or_else(|| Error::message("Service paths must be absolute Unicode paths without control characters or parent components"))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Version {
    pub version: String,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    schema: u32,
    pub install_id: String,
    user: String,
    uid: u32,
    gid: u32,
    name: String,
    config: PathBuf,
    state: PathBuf,
    server: String,
    pub current: Version,
    previous: Option<Version>,
    #[serde(default)]
    migrated: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    schema: u32,
    old: Record,
    new: Record,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn version_path(prefix: &Path, version: &Version) -> Result<PathBuf> {
    if !crate::update::valid_version(&version.version)
        || version.bytes == 0
        || version.bytes > crate::update::MAX_BINARY_BYTES
        || version.sha256.len() != 64
        || !version
            .sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::message("Invalid system service version record"));
    }
    Ok(prefix.join("versions").join(&version.version).join("rdh"))
}
fn verify(prefix: &Path, version: &Version) -> Result<PathBuf> {
    let binary = version_path(prefix, version)?;
    let bytes = store::read(&binary, crate::update::MAX_BINARY_BYTES)?;
    if bytes.len() as u64 != version.bytes
        || digest(&bytes) != version.sha256
        || fs::symlink_metadata(&binary)?.mode() & 0o111 != 0o111
    {
        return Err(Error::message(
            "System service executable size, checksum or permissions changed",
        ));
    }
    crate::native_install::check_executable(&binary)?;
    Ok(binary)
}
fn validate_record(prefix: &Path, record: &Record) -> Result<()> {
    if record.schema != 1
        || record.uid == 0
        || record.gid == 0
        || record.install_id.len() != 32
        || !record.install_id.bytes().all(|b| b.is_ascii_hexdigit())
        || !crate::startup::valid_name(&record.name)
    {
        return Err(Error::message("Invalid system service installation record"));
    }
    #[cfg(target_os = "linux")]
    if !record.name.ends_with(".service") {
        return Err(Error::message(
            "A systemd service name must end in .service",
        ));
    }
    path_text(&record.config)?;
    path_text(&record.state)?;
    crate::config::validate_gateway(&record.server)?;
    version_path(prefix, &record.current)?;
    if let Some(previous) = &record.previous {
        version_path(prefix, previous)?;
    }
    Ok(())
}
fn read_record(prefix: &Path) -> Result<Record> {
    let record = serde_json::from_slice(&store::read(&prefix.join(RECORD), RECORD_LIMIT)?)?;
    validate_record(prefix, &record)?;
    Ok(record)
}
fn pointer(prefix: &Path) -> Result<String> {
    let path = fs::read_link(prefix.join("current"))?;
    let mut parts = path.components();
    if parts.next() != Some(Component::Normal("versions".as_ref())) {
        return Err(Error::message("Invalid system service current pointer"));
    }
    let version = match parts.next() {
        Some(Component::Normal(v)) => v.to_str().unwrap_or(""),
        _ => "",
    };
    if parts.next().is_some() || !crate::update::valid_version(version) {
        return Err(Error::message("Invalid system service current pointer"));
    }
    Ok(version.into())
}
fn set_pointer(prefix: &Path, version: &Version) -> Result<()> {
    verify(prefix, version)?;
    let temporary = prefix.join(format!(".current-{}", crate::random::hex_token::<16>()?));
    symlink(format!("versions/{}", version.version), &temporary)?;
    if let Err(error) = fs::rename(&temporary, prefix.join("current")) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    store::sync(prefix)
}
fn current_prefix() -> Result<Option<(PathBuf, PathBuf)>> {
    let binary = std::env::current_exe()?.canonicalize()?;
    let Some(versions) = binary
        .parent()
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == "versions"))
    else {
        return Ok(None);
    };
    let prefix = versions
        .parent()
        .ok_or_else(|| Error::message("Invalid system service executable path"))?;
    if !exists(&prefix.join(RECORD))? && !exists(&prefix.join(INSTALL_PENDING))? {
        return Ok(None);
    }
    Ok(Some((prefix.into(), binary)))
}
fn installed_path(prefix: &Path, record: &Record, executable: &Path) -> Result<()> {
    let selected = verify(prefix, &record.current)?;
    let expected = fs::metadata(&selected)?;
    let actual = fs::metadata(executable)?;
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        return Err(Error::message("Use the current system service executable"));
    }
    Ok(())
}

pub(crate) struct Lease {
    prefix: PathBuf,
    executable: PathBuf,
    _lock: store::Lock,
    pub record: Record,
    exclusive: bool,
}
impl Lease {
    pub fn acquire_current(exclusive: bool, allow_pending: bool) -> Result<Option<Self>> {
        let Some((prefix, executable)) = current_prefix()? else {
            return Ok(None);
        };
        Self::acquire(prefix, executable, exclusive, allow_pending).map(Some)
    }
    fn acquire(
        prefix: PathBuf,
        executable: PathBuf,
        exclusive: bool,
        allow_pending: bool,
    ) -> Result<Self> {
        let lock = store::Lock::acquire(&prefix.join(LOCK), exclusive)?;
        if exists(&prefix.join(INSTALL_PENDING))? {
            return Err(Error::message(
                "System service installation is incomplete; inspect its retained program directory and registration before retrying",
            ));
        }
        let pending_exists = exists(&prefix.join(PENDING))?;
        if !allow_pending && pending_exists {
            return Err(Error::message(
                "A system service update was interrupted; use upgrade --recover before starting the Agent",
            ));
        }
        let record = read_record(&prefix)?;
        if !allow_pending || !pending_exists {
            if pointer(&prefix)? != record.current.version {
                return Err(Error::message(
                    "System service pointer disagrees with its installation record",
                ));
            }
            installed_path(&prefix, &record, &executable)?;
        } else {
            // Recovery may be invoked by either side of the recorded switch.
            let actual = fs::metadata(&executable)?;
            let pending = read_pending(&prefix)?;
            let old = fs::metadata(verify(&prefix, &pending.old.current)?)?;
            let new = fs::metadata(verify(&prefix, &pending.new.current)?)?;
            if ![(old.dev(), old.ino()), (new.dev(), new.ino())]
                .contains(&(actual.dev(), actual.ino()))
            {
                return Err(Error::message(
                    "Recovery executable is not part of the recorded update",
                ));
            }
        }
        Ok(Self {
            prefix,
            executable,
            _lock: lock,
            record,
            exclusive,
        })
    }
    pub fn exclusive(self, allow_pending: bool) -> Result<Self> {
        let prefix = self.prefix.clone();
        let executable = self.executable.clone();
        let expected = self.record.clone();
        drop(self);
        let manager = Self::acquire(prefix, executable, true, allow_pending)?;
        if manager.record != expected {
            return Err(Error::message(
                "System service installation changed while checking the release; inspect it before retrying",
            ));
        }
        Ok(manager)
    }
    fn account(&self) -> Result<Account> {
        let account = Account::lookup(&self.record.user)?;
        if account.uid != self.record.uid || account.gid != self.record.gid {
            return Err(Error::message(
                "Service account identity changed; inspect it before updating",
            ));
        }
        Ok(account)
    }
    fn profile(&self, candidate: &Path, version: &str) -> Result<ProfileLease> {
        let lease = ProfileLease::start(
            candidate,
            &self.account()?,
            &self.record.config,
            &self.record.state,
            &self.record.name,
            version,
        )?;
        if lease.ready.config != self.record.config
            || lease.ready.state != self.record.state
            || lease.ready.server != self.record.server
            || !lease.ready.bound
        {
            return Err(Error::message(
                "Service profile identity or binding changed; inspect it before updating",
            ));
        }
        Ok(lease)
    }
    fn offline(&self) -> Result<()> {
        if !self.exclusive {
            return Err(Error::message(
                "System service update requires its exclusive lock",
            ));
        }
        store::root()?;
        registration_matches(&self.prefix, &self.record)?;
        stopped(&self.record)
    }
    pub fn stage_and_switch(&self, version: &str, bytes: &[u8], sha256: &str) -> Result<()> {
        self.offline()?;
        let selected = Version {
            version: version.into(),
            bytes: bytes.len() as u64,
            sha256: sha256.into(),
        };
        let candidate = stage(&self.prefix, &selected, bytes)?;
        if selected == self.record.current {
            return Ok(());
        }
        let mut profile = self.profile(&candidate, version)?;
        let mut next = self.record.clone();
        next.previous = Some(next.current.clone());
        next.current = selected;
        self.switch(&next, &mut profile)?;
        profile.finish()
    }
    pub fn rollback(&self) -> Result<()> {
        self.offline()?;
        let previous =
            self.record.previous.as_ref().ok_or_else(|| {
                Error::message("This service installation has no previous version")
            })?;
        let binary = verify(&self.prefix, previous)?;
        let mut profile = self.profile(&binary, &previous.version)?;
        let mut next = self.record.clone();
        next.current = previous.clone();
        next.previous = Some(self.record.current.clone());
        self.switch(&next, &mut profile)?;
        profile.finish()
    }
    fn switch(&self, next: &Record, profile: &mut ProfileLease) -> Result<()> {
        self.offline()?;
        if exists(&self.prefix.join(PENDING))?
            || read_record(&self.prefix)? != self.record
            || pointer(&self.prefix)? != self.record.current.version
        {
            return Err(Error::message(
                "System service state changed before switching",
            ));
        }
        verify(&self.prefix, &self.record.current)?;
        profile.check()?;
        store::create(
            &self.prefix.join(PENDING),
            &serde_json::to_vec(&Pending {
                schema: 1,
                old: self.record.clone(),
                new: next.clone(),
            })?,
            false,
        )?;
        set_pointer(&self.prefix, &next.current)?;
        store::replace(&self.prefix.join(RECORD), &serde_json::to_vec(next)?)?;
        fs::remove_file(self.prefix.join(PENDING))?;
        store::sync(&self.prefix)
    }
    pub fn recover(&self) -> Result<()> {
        self.offline()?;
        if !exists(&self.prefix.join(PENDING))? {
            return Ok(());
        }
        let pending = read_pending(&self.prefix)?;
        let current = pointer(&self.prefix)?;
        if (self.record != pending.old && self.record != pending.new)
            || ![&pending.old.current.version, &pending.new.current.version].contains(&&current)
        {
            return Err(Error::message(
                "Service state changed outside the interrupted update; recovery refused",
            ));
        }
        let binary = verify(&self.prefix, &pending.old.current)?;
        let mut profile = self.profile(&binary, &pending.old.current.version)?;
        profile.check()?;
        set_pointer(&self.prefix, &pending.old.current)?;
        store::replace(
            &self.prefix.join(RECORD),
            &serde_json::to_vec(&pending.old)?,
        )?;
        fs::remove_file(self.prefix.join(PENDING))?;
        store::sync(&self.prefix)?;
        profile.finish()
    }
}
fn read_pending(prefix: &Path) -> Result<Pending> {
    let pending: Pending =
        serde_json::from_slice(&store::read(&prefix.join(PENDING), 2 * RECORD_LIMIT)?)?;
    validate_record(prefix, &pending.old)?;
    validate_record(prefix, &pending.new)?;
    let mut expected = pending.old.clone();
    expected.previous = Some(expected.current.clone());
    expected.current = pending.new.current.clone();
    if pending.schema != 1 || pending.new != expected {
        return Err(Error::message(
            "Invalid interrupted system service update record",
        ));
    }
    Ok(pending)
}
fn stage(prefix: &Path, version: &Version, bytes: &[u8]) -> Result<PathBuf> {
    let binary = version_path(prefix, version)?;
    if bytes.len() as u64 != version.bytes || digest(bytes) != version.sha256 {
        return Err(Error::message(
            "System service update checksum does not match",
        ));
    }
    if exists(binary.parent().unwrap())? {
        return verify(prefix, version);
    }
    let versions = prefix.join("versions");
    let temporary = versions.join(format!(".stage-{}", crate::random::hex_token::<16>()?));
    store::create_directory(&temporary)?;
    store::create(&temporary.join("rdh"), bytes, true)?;
    crate::native_install::check_executable(&temporary.join("rdh"))?;
    // The exclusive installation lock serializes publishers. Root is trusted;
    // a different existing version is never overwritten.
    if exists(binary.parent().unwrap())? {
        return Err(Error::message(
            "System service version appeared during staging",
        ));
    }
    fs::rename(&temporary, binary.parent().unwrap())?;
    store::sync(&versions)?;
    verify(prefix, version)
}
pub(crate) fn runtime_guard(args: &LaunchArgs, state: Option<&Path>) -> Result<Option<Lease>> {
    let Some(lease) = Lease::acquire_current(false, false)? else {
        return Ok(None);
    };
    let record = &lease.record;
    if unsafe { libc::getuid() } != record.uid
        || unsafe { libc::getgid() } != record.gid
        || args
            .config
            .as_deref()
            .is_none_or(|p| Path::new(p) != record.config)
        || state != Some(record.state.as_path())
        || args.server != record.server
        || args.pair_only
        || args.name.is_some()
        || !args.roots.is_empty()
    {
        return Err(Error::message(
            "System service launch must use the recorded account, configuration, server and state directory",
        ));
    }
    Ok(Some(lease))
}
pub(crate) fn check_service_entry(
    name: &str,
    args: &LaunchArgs,
    state: Option<&Path>,
) -> Result<()> {
    let lease = runtime_guard(args, state)?.ok_or_else(|| {
        Error::message("POSIX service-run requires a managed system service installation")
    })?;
    if lease.record.name != name {
        return Err(Error::message(
            "System service name does not match its installation",
        ));
    }
    Ok(())
}

fn manager(args: &[&str]) -> Result<(std::process::ExitStatus, String)> {
    #[cfg(target_os = "linux")]
    let program = "/usr/bin/systemctl";
    #[cfg(target_os = "macos")]
    let program = "/bin/launchctl";
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .args(args);
    crate::startup_native::posix::run_command(command, Duration::from_secs(15))
}
fn checked_manager(args: &[&str]) -> Result<()> {
    if !manager(args)?.0.success() {
        return Err(Error::message(
            "System service manager did not confirm the operation; inspect the retained installation",
        ));
    }
    Ok(())
}
fn registration(record: &Record) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/etc/systemd/system").join(&record.name)
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/LaunchDaemons").join(format!("{}.plist", record.name))
    }
}
fn arguments(prefix: &Path, record: &Record) -> Result<Vec<String>> {
    Ok(vec![
        path_text(&prefix.join("current/rdh"))?.into(),
        "service-run".into(),
        "--service-name".into(),
        record.name.clone(),
        "--config".into(),
        path_text(&record.config)?.into(),
        "--server".into(),
        record.server.clone(),
        "--state-directory".into(),
        path_text(&record.state)?.into(),
        "--recover-lock".into(),
    ])
}
#[cfg(target_os = "linux")]
fn systemd_executable(path: &Path) -> Result<()> {
    // systemd 255 checks string_is_safe after unquoting/specifier expansion.
    // Quotes and backslashes in the executable itself cannot be escaped past
    // that check. Other arguments retain their separate quoted encoding.
    if path_text(path)?
        .chars()
        .any(|c| matches!(c, '\'' | '"' | '\\'))
    {
        return Err(Error::message(
            "Systemd executable paths cannot contain quotes or backslashes; choose another program prefix",
        ));
    }
    Ok(())
}
fn definition(prefix: &Path, record: &Record) -> Result<Vec<u8>> {
    let account = Account::lookup(&record.user)?;
    if account.uid != record.uid || account.gid != record.gid {
        return Err(Error::message("Service account changed"));
    }
    let args = arguments(prefix, record)?;
    #[cfg(target_os = "linux")]
    {
        systemd_executable(Path::new(&args[0]))?;
        let quote = |s: &str| {
            format!(
                "\"{}\"",
                s.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%")
            )
        };
        let command = args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ");
        let working = path_text(&record.state)?;
        if working.ends_with(char::is_whitespace) || working.ends_with('\\') {
            return Err(Error::message(
                "Systemd working directory cannot end in whitespace or a backslash",
            ));
        }
        // WorkingDirectory accepts the entire unquoted value, unlike an
        // ExecStart word. ':' disables environment expansion for our literal
        // argument vector, including dollar signs in executable path names.
        Ok(format!("[Unit]\nDescription=Remote Device Hub Agent\nAfter=network-online.target\n\n[Service]\nType=simple\nUser={}\nGroup={}\nEnvironment={}\nWorkingDirectory={}\nExecStart=:{}\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n", record.uid, record.gid, quote(&format!("HOME={}", path_text(&account.home)?)), working.replace('%', "%%"), command).into_bytes())
    }
    #[cfg(target_os = "macos")]
    {
        let quote = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let command = args
            .iter()
            .map(|s| format!("<string>{}</string>", quote(s)))
            .collect::<String>();
        let disabled = if record.migrated { "false" } else { "true" };
        Ok(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>UserName</key><string>{}</string>\n<key>ProgramArguments</key><array>{}</array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string></dict>\n<key>RunAtLoad</key><true/>\n<key>Disabled</key><{disabled}/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict></plist>\n", quote(&record.name), quote(&record.user), command, quote(path_text(&record.state)?), quote(path_text(&account.home)?)).into_bytes())
    }
}
fn registration_matches(prefix: &Path, record: &Record) -> Result<()> {
    if store::read(&registration(record), RECORD_LIMIT)? != definition(prefix, record)? {
        return Err(Error::message(
            "System service definition changed outside this installation",
        ));
    }
    Ok(())
}
fn stopped(record: &Record) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let (status, output) = manager(&[
            "show",
            "--all",
            &record.name,
            "--property=LoadState,ActiveState,SubState,MainPID,ControlPID,ControlGroup,DropInPaths",
        ])?;
        let map: std::collections::BTreeMap<_, _> =
            output.lines().filter_map(|l| l.split_once('=')).collect();
        if !status.success()
            || map.get("LoadState") != Some(&"loaded")
            || map.get("ActiveState") != Some(&"inactive")
            || map.get("SubState") != Some(&"dead")
            || map.get("MainPID") != Some(&"0")
            || map.get("ControlPID") != Some(&"0")
            || map.get("ControlGroup") != Some(&"")
            || map.get("DropInPaths") != Some(&"")
        {
            return Err(Error::message(
                "Stop the system service and remove unexpected overrides before an offline update",
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let (status, output) = manager(&["print", &format!("system/{}", record.name)])?;
        // A known definition plus ESRCH (113) means it was booted out. Unknown
        // manager failures do not prove that the service is stopped.
        if status.code() == Some(113) {
            return Ok(());
        }
        if !status.success()
            || !output.lines().any(|l| l.trim() == "state = not running")
            || output.lines().any(|l| l.trim_start().starts_with("pid ="))
        {
            return Err(Error::message(
                "Boot out the system LaunchDaemon before an offline update",
            ));
        }
    }
    Ok(())
}
fn ensure_absent(record: &Record) -> Result<()> {
    if exists(&registration(record))? {
        return Err(Error::message(
            "System service registration already exists; migrate it explicitly",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let (status, output) = manager(&["show", &record.name, "--property=LoadState", "--value"])?;
        if !status.success() || output != "not-found" {
            return Err(Error::message(
                "System service name is already registered or its state is unavailable",
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if manager(&["print", &format!("system/{}", record.name)])?
            .0
            .code()
            != Some(113)
        {
            return Err(Error::message(
                "System LaunchDaemon name is loaded or its state is unavailable",
            ));
        }
    }
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<()> {
    if let [flag, rest @ ..] = args
        && flag == "--profile-exec"
    {
        return profile::exec_as_user(rest);
    }
    if let [flag, rest @ ..] = args
        && flag == "--profile-lease"
    {
        return profile::helper(rest);
    }
    store::root()?;
    if migration::resume(args)? {
        return Ok(());
    }
    let mut options = std::collections::BTreeMap::new();
    let mut items = args.iter();
    while let Some(flag) = items.next() {
        if !matches!(
            flag.as_str(),
            "--prefix"
                | "--config"
                | "--state-directory"
                | "--service-user"
                | "--name"
                | "--migrate-from"
        ) {
            return Err(Error::message("Invalid POSIX service installation option"));
        }
        let value = items
            .next()
            .filter(|v| !v.is_empty() && !v.starts_with("--"))
            .ok_or_else(|| Error::message(format!("{flag} requires a value")))?;
        if options.insert(flag.as_str(), value.as_str()).is_some() {
            return Err(Error::message("Duplicate service installation option"));
        }
    }
    let required = |key| {
        options
            .get(key)
            .copied()
            .ok_or_else(|| Error::message(format!("install-service requires {key}")))
    };
    let account = Account::lookup(required("--service-user")?)?;
    let prefix = store::parent_for_new(Path::new(required("--prefix")?))?;
    #[cfg(target_os = "linux")]
    systemd_executable(&prefix)?;
    let config = Path::new(required("--config")?);
    let state = Path::new(required("--state-directory")?);
    let name = required("--name")?;
    if !crate::startup::valid_name(name) {
        return Err(Error::message("Invalid system service name"));
    }
    #[cfg(target_os = "linux")]
    if !name.ends_with(".service") {
        return Err(Error::message("Use a systemd name ending in .service"));
    }
    path_text(config)?;
    path_text(state)?;
    if let Some(legacy) = options.get("--migrate-from") {
        path_text(Path::new(legacy))?;
        let legacy = Path::new(legacy).canonicalize()?;
        store::directory(&legacy)?;
        if prefix.starts_with(&legacy) || legacy.starts_with(&prefix) {
            return Err(Error::message(
                "Use separate native and legacy program directories",
            ));
        }
    }
    let source = std::env::current_exe()?.canonicalize()?;
    // The invoked executable is already trusted by the administrator; copy it
    // before dropping privilege so a protected executable path is passed on.
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(&source)?
        .take(crate::update::MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let version = Version {
        version: env!("CARGO_PKG_VERSION").into(),
        bytes: bytes.len() as u64,
        sha256: digest(&bytes),
    };
    version_path(&prefix, &version)?;
    store::create_directory(&prefix)?;
    store::create(
        &prefix.join(INSTALL_PENDING),
        b"Installation must complete before service startup.\n",
        false,
    )?;
    store::create(&prefix.join(LOCK), b"", false)?;
    let _lock = store::Lock::acquire(&prefix.join(LOCK), true)?;
    store::create_directory(&prefix.join("versions"))?;
    let binary = stage(&prefix, &version, &bytes)?;
    let mut profile =
        ProfileLease::start(&binary, &account, config, state, name, &version.version)?;
    let legacy = options.get("--migrate-from").copied();
    if profile.ready.bound != legacy.is_some() {
        return Err(Error::message(
            "Existing service bindings require --migrate-from; migration requires the matching existing binding",
        ));
    }
    let record = Record {
        schema: 1,
        install_id: crate::random::hex_token::<16>()?,
        user: account.name,
        uid: account.uid,
        gid: account.gid,
        name: name.into(),
        config: profile.ready.config.clone(),
        state: profile.ready.state.clone(),
        server: profile.ready.server.clone(),
        current: version,
        previous: None,
        migrated: legacy.is_some(),
    };
    let legacy = legacy
        .map(|path| migration::Legacy::inspect(Path::new(path), &record, &profile.ready))
        .transpose()?;
    if legacy.is_none() {
        ensure_absent(&record)?;
    }
    let definition = definition(&prefix, &record)?;
    store::create(&prefix.join(RECORD), &serde_json::to_vec(&record)?, false)?;
    set_pointer(&prefix, &record.current)?;
    if let Some(legacy) = &legacy {
        legacy.save(&prefix)?;
        profile.check()?;
        legacy.publish(&prefix, &record)?;
    } else {
        #[cfg(target_os = "macos")]
        checked_manager(&["disable", &format!("system/{}", record.name)])?;
        store::create(&registration(&record), &definition, false)?;
        #[cfg(target_os = "linux")]
        checked_manager(&["daemon-reload"])?;
        profile.bind()?;
    }
    profile.check()?;
    profile.finish()?;
    migration::complete(&prefix)?;
    println!(
        "System service installed and bound: {}\nProgram: {}\nConfiguration retained at: {}",
        record.name,
        prefix.join("current/rdh").display(),
        record.config.display()
    );
    #[cfg(target_os = "linux")]
    println!(
        "Start locally: sudo systemctl start {}\nThen verify the device connection in the Hub.",
        record.name
    );
    #[cfg(target_os = "macos")]
    {
        if !record.migrated {
            println!(
                "Enable locally: sudo launchctl enable system/{}",
                record.name
            );
        }
        println!(
            "Start locally: sudo launchctl bootstrap system {}\nThen verify the device connection in the Hub.",
            registration(&record).display()
        );
    }
    println!(
        "{}",
        if record.migrated {
            "Existing boot startup settings were retained. The old installation is available for undo."
        } else {
            "Boot startup remains disabled until enabled locally."
        }
    );
    Ok(())
}
