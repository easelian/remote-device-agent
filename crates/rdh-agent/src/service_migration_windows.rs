//! Adopt a stopped, recognized shawl/Node LocalService registration. Only its
//! ImagePath changes. Program proofs and intent live in the administrator tree;
//! the LocalService profile is held, never copied, rewritten or re-owned.
use super::{Creation, Privilege, update};
use crate::{
    cli::{Action, parse},
    config::{DeviceConfig, local_roots, same_hub, sha256_hex},
    error::{Error, Result},
    policy::Policy,
    startup,
    startup_native::windows::{administrator, startup_path},
    storage::windows::{
        account_matches_sid, check_legacy_service_program, check_service_object,
        definitely_not_found, named_identity, open_child, open_service_data_child,
        open_service_install_parent, require_child_absent,
    },
    update::{MAX_BINARY_BYTES, valid_version},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use windows_sys::Win32::{
    Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE},
    System::Services::{
        SERVICE_AUTO_START, SERVICE_DEMAND_START, SERVICE_DISABLED, SERVICE_WIN32_OWN_PROCESS,
    },
};

const BACKUP: &str = ".service-migration.json";
const INTENT: &str = ".service-migration-intent";
const UNDONE: &str = ".service-migration-undone";
const LIMIT: u64 = 128 * 1024;
const ENTRY: &[u8] = b"#!/usr/bin/env node\n// Compiled during development/release; consumers execute JavaScript directly.\nimport '../bundle/agent.mjs';\n";

fn invalid(message: &str) -> Error {
    Error::message(format!(
        "Legacy Windows service migration refused: {message}"
    ))
}
fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
pub(super) fn require_complete(prefix: &Path) -> Result<()> {
    if exists(&prefix.join(INTENT))? || exists(&prefix.join(UNDONE))? {
        return Err(invalid(
            "native startup is blocked; inspect install-service --recover-migration or --undo-migration",
        ));
    }
    Ok(())
}
fn bytes(file: &mut File, limit: u64) -> Result<Vec<u8>> {
    if file.metadata()?.len() > limit {
        return Err(invalid("file is too large"));
    }
    let mut result = Vec::new();
    file.take(limit + 1).read_to_end(&mut result)?;
    if result.len() as u64 > limit {
        return Err(invalid("file is too large"));
    }
    Ok(result)
}
fn text(path: &Path) -> Result<String> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
    {
        return Err(invalid(
            "absolute Unicode paths without control characters are required",
        ));
    }
    Ok(startup_path(path)?.replace('/', "\\"))
}
fn nested(path: &Path, parent: &Path) -> Result<bool> {
    let parent = text(parent)?;
    for ancestor in path.ancestors() {
        if text(ancestor)?.eq_ignore_ascii_case(&parent) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn same_path(value: &str, path: &Path) -> Result<bool> {
    Ok(
        text(&Path::new(value).canonicalize()?)?
            .eq_ignore_ascii_case(&text(&path.canonicalize()?)?),
    )
}

// Decode only the installer's always-quoted CommandLineToArgvW form. Requiring
// byte-for-byte re-encoding excludes concatenated, unquoted and alternate forms.
fn arguments(command: &str) -> Result<Vec<String>> {
    if command.len() > 32768 || command.chars().any(char::is_control) {
        return Err(invalid("invalid legacy command size or characters"));
    }
    let mut rest = command;
    let mut result = Vec::new();
    while !rest.is_empty() {
        rest = rest
            .strip_prefix('"')
            .ok_or_else(|| invalid("unrecognized legacy quoting"))?;
        let mut word = String::new();
        let mut slashes = 0;
        let mut end = None;
        for (offset, ch) in rest.char_indices() {
            if ch == '\\' {
                slashes += 1;
                continue;
            }
            if ch == '"' {
                word.extend(std::iter::repeat_n('\\', slashes / 2));
                if slashes % 2 == 0 {
                    end = Some(offset + 1);
                    break;
                }
                word.push('"');
            } else {
                word.extend(std::iter::repeat_n('\\', slashes));
                word.push(ch);
            }
            slashes = 0;
        }
        rest = &rest[end.ok_or_else(|| invalid("unterminated legacy argument"))?..];
        result.push(word);
        if result.len() > 160 {
            return Err(invalid("too many legacy arguments"));
        }
        if !rest.is_empty() {
            rest = rest
                .strip_prefix(' ')
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("unrecognized legacy separator"))?;
        }
    }
    let encoded = result
        .iter()
        .map(|s| crate::processes::windows_argument(s))
        .collect::<Vec<_>>()
        .join(" ");
    if encoded != command {
        return Err(invalid("legacy command is not in installer format"));
    }
    Ok(result)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Proof {
    path: PathBuf,
    sha256: String,
    bytes: u64,
}
struct HeldProgram {
    _files: Vec<File>,
    proofs: Vec<Proof>,
    directory: PathBuf,
}
fn program_file(prefix: &Path, path: &Path, limit: u64, held: &mut Vec<File>) -> Result<Vec<u8>> {
    let (parents, name) = open_service_install_parent(path)?;
    let depth = path
        .strip_prefix(prefix)
        .map_err(|_| invalid("program escaped the legacy prefix"))?
        .components()
        .count();
    if depth == 0 || depth > parents.len() {
        return Err(invalid("invalid program path"));
    }
    for parent in parents.iter().rev().take(depth) {
        check_legacy_service_program(parent)?;
    }
    let mut file = open_child(parents.last().unwrap(), &name, false, true)?;
    check_legacy_service_program(&file)?;
    let result = bytes(&mut file, limit)?;
    held.extend(parents);
    held.push(file); // No write/delete sharing throughout the SCM mutation.
    Ok(result)
}
fn program(prefix: &Path, command: &str) -> Result<HeldProgram> {
    let args = arguments(command)?;
    let shawl = PathBuf::from(
        args.first()
            .ok_or_else(|| invalid("missing shawl program"))?,
    );
    let directory = shawl
        .parent()
        .ok_or_else(|| invalid("missing legacy version directory"))?
        .canonicalize()?;
    if directory.parent() != Some(prefix.join("versions").as_path())
        || shawl.file_name().and_then(|s| s.to_str()) != Some("shawl.exe")
    {
        return Err(invalid(
            "legacy service must use a versioned installer package",
        ));
    }
    let mut held = Vec::new();
    let mut proofs = Vec::new();
    let mut package_version = String::new();
    for (relative, limit) in [
        ("shawl.exe", MAX_BINARY_BYTES),
        ("node.exe", MAX_BINARY_BYTES),
        ("bin/remote-device-hub.mjs", LIMIT),
        ("bundle/agent.mjs", MAX_BINARY_BYTES),
        ("package.json", LIMIT),
    ] {
        let path = directory.join(relative);
        let content = program_file(prefix, &path, limit, &mut held)?;
        if content.is_empty() {
            return Err(invalid("empty legacy program file"));
        }
        if relative.ends_with(".exe") {
            crate::native_install::check_executable(&content)?;
        }
        if relative == "bin/remote-device-hub.mjs" && content != ENTRY {
            return Err(invalid("unrecognized legacy entry point"));
        }
        if relative == "package.json" {
            let value: serde_json::Value = serde_json::from_slice(&content)?;
            let version = value["version"]
                .as_str()
                .filter(|v| valid_version(v))
                .ok_or_else(|| invalid("invalid legacy package version"))?;
            if value["name"] != "remote-device-hub" {
                return Err(invalid("unrecognized legacy package"));
            }
            package_version = version.into();
        }
        proofs.push(Proof {
            path,
            sha256: sha256_hex(&content),
            bytes: content.len() as u64,
        });
    }
    let architecture = if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "arm64"
    };
    if directory.file_name().and_then(|s| s.to_str())
        != Some(&format!("rdh-{package_version}-win32-{architecture}"))
    {
        return Err(invalid("legacy package directory and version differ"));
    }
    Ok(HeldProgram {
        _files: held,
        proofs,
        directory,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProfileProof {
    config: String,
    startup: String,
    policy: Option<String>,
}
struct Profile {
    _held: Vec<File>,
    directory: File,
    policy_absent: bool,
    proof: ProfileProof,
    config: DeviceConfig,
}
fn data_file(directory: &File, name: &str, limit: u64, held: &mut Vec<File>) -> Result<Vec<u8>> {
    let mut file = open_service_data_child(
        directory,
        &name.encode_utf16().collect::<Vec<_>>(),
        false,
        0,
    )?;
    check_service_object(&file, false)?;
    let result = bytes(&mut file, limit)?;
    held.push(file);
    Ok(result)
}
impl Profile {
    fn acquire(config: &Path, service: &str) -> Result<Self> {
        if config.file_name().and_then(|s| s.to_str()) != Some("agent.json") {
            return Err(invalid("expected the installer's agent.json profile"));
        }
        let data = config
            .parent()
            .ok_or_else(|| invalid("missing service data directory"))?;
        let (mut held, name) = open_service_install_parent(data)?;
        // Old and native lock files may grant only LocalService access. The
        // elevated caller reads them with backup intent, preserving their ACL.
        let mut privilege = Privilege::backup_read()?;
        let directory = open_service_data_child(
            held.last().unwrap(),
            &name,
            true,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        check_service_object(&directory, false)?;
        // Acquire every profile file before inspecting owners. An already
        // connected Agent is then caught by its live lock owner; a later one
        // cannot read these files. A crash releases the kernel exclusion.
        let config_bytes = data_file(&directory, "agent.json", 4 * 1024 * 1024, &mut held)?;
        let startup_bytes = data_file(&directory, "agent.json.startup.json", 65536, &mut held)?;
        let policy_bytes = match data_file(&directory, "agent.policy.json", 65536, &mut held) {
            Ok(value) => Some(value),
            Err(Error::Io(error)) if definitely_not_found(&error) => None,
            Err(error) => return Err(error),
        };
        let mut owners = Vec::new();
        for name in ["agent.json.node-lock", "agent.json.startup.json.node-lock"] {
            require_child_absent(
                &directory,
                &format!("{name}.reclaiming")
                    .encode_utf16()
                    .collect::<Vec<_>>(),
                true,
            )?;
            let lock = match open_service_data_child(
                &directory,
                &name.encode_utf16().collect::<Vec<_>>(),
                true,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            ) {
                Ok(value) => value,
                Err(Error::Io(error)) if definitely_not_found(&error) => continue,
                Err(error) => return Err(error),
            };
            check_service_object(&lock, false)?;
            let owner = data_file(&lock, "owner.json", 4096, &mut held)?;
            let value: serde_json::Value = serde_json::from_slice(&owner)?;
            let pid = value["pid"]
                .as_u64()
                .filter(|pid| *pid > 0 && *pid <= u64::from(u32::MAX))
                .ok_or_else(|| invalid("incomplete legacy lock owner"))?;
            if value["nonce"]
                .as_str()
                .is_none_or(|s| s.encode_utf16().count() < 32)
            {
                return Err(invalid("incomplete legacy lock nonce"));
            }
            owners.push(pid as u32);
            held.push(lock);
        }
        // All required data handles are retained. No candidate or service
        // process may inherit the temporary backup privilege.
        privilege.restore()?;
        drop(privilege);
        crate::profile_lock::require_dead_owners(&owners)?;
        let config = DeviceConfig::parse(serde_json::from_slice(&config_bytes)?)?;
        startup::check_service_binding_bytes(&startup_bytes, service)?;
        let policy = if let Some(value) = &policy_bytes {
            Policy::parse(&serde_json::from_slice(value)?, &config)?
        } else {
            Policy::default()
        };
        policy.ensure_sandbox_supported()?;
        let result = Self {
            _held: held,
            directory,
            policy_absent: policy_bytes.is_none(),
            config,
            proof: ProfileProof {
                config: sha256_hex(&config_bytes),
                startup: sha256_hex(&startup_bytes),
                policy: policy_bytes.as_ref().map(|b| sha256_hex(b)),
            },
        };
        result.check()?;
        Ok(result)
    }
    fn check(&self) -> Result<()> {
        if self.policy_absent
            && named_identity(
                &self.directory,
                &"agent.policy.json".encode_utf16().collect::<Vec<_>>(),
            )?
            .is_some()
        {
            return Err(invalid("service policy appeared during migration"));
        }
        Ok(())
    }
}

fn check_command(
    before: &update::ServiceConfig,
    service: &str,
    program: &HeldProgram,
    config_path: &Path,
    paired: &DeviceConfig,
) -> Result<()> {
    if before.service_type != SERVICE_WIN32_OWN_PROCESS
        || ![SERVICE_AUTO_START, SERVICE_DEMAND_START, SERVICE_DISABLED]
            .contains(&before.start_type)
        || !account_matches_sid(&before.account, "S-1-5-19")?
    {
        return Err(invalid(
            "service must be an own-process LocalService registration",
        ));
    }
    let args = arguments(&before.binary_path)?;
    let data = text(config_path.parent().unwrap())?;
    let fixed = [
        text(&program.directory.join("shawl.exe"))?,
        "run".into(),
        "--name".into(),
        service.into(),
        "--cwd".into(),
        data.clone(),
        "--log-dir".into(),
        data.clone(),
        "--env".into(),
        format!("XDG_STATE_HOME={data}"),
        "--stop-timeout".into(),
        "15000".into(),
        "--kill-process-tree".into(),
        "--restart".into(),
        "--restart-delay".into(),
        "5000".into(),
        "--".into(),
        text(&program.directory.join("node.exe"))?,
        text(&program.directory.join("bin/remote-device-hub.mjs"))?,
    ];
    if !args.starts_with(&fixed) {
        return Err(invalid("unrecognized shawl launch command"));
    }
    let launch_args = &args[fixed.len()..];
    let Action::Launch(launch) = parse(launch_args, None)? else {
        return Err(invalid("unexpected legacy operation"));
    };
    if launch.pair_only
        || launch.name.is_some()
        || !same_hub(&launch.server, &paired.gateway_url)
        || !same_path(
            launch
                .config
                .as_deref()
                .ok_or_else(|| invalid("missing legacy profile"))?,
            config_path,
        )?
        || launch_args
            .iter()
            .filter(|s| s.as_str() == "--config")
            .count()
            != 1
        || launch_args
            .iter()
            .filter(|s| s.as_str() == "--server")
            .count()
            > 1
        || !launch_args.ends_with(&["--no-browser".into(), "--recover-lock".into()])
    {
        return Err(invalid("legacy arguments differ from the paired profile"));
    }
    if !launch.roots.is_empty() && local_roots(&launch.roots)? != paired.roots {
        return Err(invalid("legacy roots differ from the paired profile"));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Backup {
    schema: u8,
    legacy: PathBuf,
    native: update::Record,
    before: update::ServiceConfig,
    files: Vec<Proof>,
    profile: ProfileProof,
}
impl Backup {
    fn recheck(&self, current: &update::Record) -> Result<(HeldProgram, Profile)> {
        if self.schema != 1
            || !self.legacy.is_absolute()
            || !self.native.same_installation(current)?
        {
            return Err(invalid("invalid migration installation record"));
        }
        let program = program(&self.legacy, &self.before.binary_path)?;
        let profile = Profile::acquire(&current.config, &current.service)?;
        check_command(
            &self.before,
            &current.service,
            &program,
            &current.config,
            &profile.config,
        )?;
        if self.files != program.proofs
            || self.profile != profile.proof
            || !same_hub(&current.server, &profile.config.gateway_url)
        {
            return Err(invalid("legacy program or original profile changed"));
        }
        Ok((program, profile))
    }
    fn after(&self, prefix: &Path, current: &update::Record) -> Result<update::ServiceConfig> {
        let mut after = self.before.clone();
        after.binary_path = current.command(prefix)?;
        Ok(after)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Intent {
    Apply,
    Undo,
}

fn resume(prefix: &Path, undo: bool) -> Result<()> {
    let prefix = prefix.canonicalize()?;
    let _lock = update::manager_lock(&prefix, true)?;
    if exists(&prefix.join(update::PENDING))? {
        return Err(invalid("recover the native version update first"));
    }
    let record: update::Record = update::read_record(&prefix, update::MANIFEST)?;
    let backup: Backup = update::read_record(&prefix, BACKUP)?;
    let (_program, profile) = backup.recheck(&record)?;
    let _binary = update::verify_binary(&prefix, &record.current)?;
    let _original_binary = update::verify_binary(&prefix, &backup.native.current)?;
    let service = update::Control::open(&record.service, true)?;
    service.stopped()?;
    let observed = service.configuration()?;
    let after = backup.after(&prefix, &record)?;
    if observed != backup.before && observed != after {
        return Err(invalid(
            "service registration changed outside this migration",
        ));
    }
    let intent: Option<Intent> = if exists(&prefix.join(INTENT))? {
        Some(update::read_record(&prefix, INTENT)?)
    } else {
        None
    };
    let desired = if undo {
        if intent == Some(Intent::Apply) {
            return Err(invalid(
                "finish the interrupted migration before undoing it",
            ));
        }
        Intent::Undo
    } else if let Some(intent) = intent {
        intent
    } else {
        if exists(&prefix.join(UNDONE))? && observed == backup.before
            || !exists(&prefix.join(UNDONE))? && observed == after
        {
            return Ok(());
        }
        return Err(invalid(
            "registration and completed migration state disagree",
        ));
    };
    if desired == Intent::Apply && exists(&prefix.join(UNDONE))? {
        return Err(invalid("migration was already undone"));
    }
    if !exists(&prefix.join(INTENT))? {
        update::write_record(&prefix, INTENT, &desired, false)?;
    }
    profile.check()?;
    let target = if desired == Intent::Undo {
        &backup.before
    } else {
        &after
    };
    if &observed != target {
        service.replace(&observed, target)?;
    }
    service.stopped()?;
    profile.check()?;
    if service.configuration()? != *target {
        return Err(invalid("service changed before completing migration"));
    }
    if desired == Intent::Undo && !exists(&prefix.join(UNDONE))? {
        update::write_record(&prefix, UNDONE, &Intent::Undo, false)?;
    }
    update::remove_record(&prefix, INTENT, &desired)?;
    println!(
        "{} service registration is ready for manual start and Hub verification; original files and identity were retained.",
        if desired == Intent::Undo {
            "Legacy"
        } else {
            "Native"
        }
    );
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<bool> {
    if !args.iter().any(|s| {
        matches!(
            s.as_str(),
            "--migrate-from" | "--recover-migration" | "--undo-migration"
        )
    }) {
        return Ok(false);
    }
    if !administrator() {
        return Err(invalid("an elevated administrator is required"));
    }
    if let [operation, flag, prefix] = args
        && matches!(
            operation.as_str(),
            "--recover-migration" | "--undo-migration"
        )
    {
        if flag != "--prefix" {
            return Err(invalid(
                "use --recover-migration|--undo-migration --prefix NATIVE_INSTALL",
            ));
        }
        text(Path::new(prefix))?;
        resume(Path::new(prefix), operation == "--undo-migration")?;
        return Ok(true);
    }
    let mut remaining = Vec::new();
    let mut legacy = None;
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if arg == "--migrate-from" {
            if legacy.is_some() {
                return Err(invalid("duplicate legacy prefix"));
            }
            legacy = Some(PathBuf::from(
                items
                    .next()
                    .ok_or_else(|| invalid("missing legacy prefix"))?,
            ));
        } else {
            remaining.push(arg.clone());
        }
    }
    let options = super::options(&remaining)?;
    let legacy = legacy.ok_or_else(|| invalid("missing legacy prefix"))?;
    text(&legacy)?;
    let legacy = legacy.canonicalize()?;
    let data = options.data.canonicalize()?;
    let config = options.config.canonicalize()?;
    if config != data.join("agent.json") {
        return Err(invalid("config must be DATA/agent.json"));
    }
    if exists(&options.prefix)? {
        return Err(invalid(
            "native prefix already exists; use --recover-migration or --undo-migration",
        ));
    }
    // Canonicalize the existing parent before enforcing disjoint trees.
    let parent = options
        .prefix
        .parent()
        .ok_or_else(|| invalid("invalid native prefix"))?
        .canonicalize()?;
    let prefix = parent.join(
        options
            .prefix
            .file_name()
            .ok_or_else(|| invalid("invalid native prefix"))?,
    );
    if nested(&prefix, &legacy)?
        || nested(&legacy, &prefix)?
        || nested(&prefix, &data)?
        || nested(&data, &prefix)?
        || nested(&legacy, &data)?
        || nested(&data, &legacy)?
    {
        return Err(invalid(
            "native program, old program and service data must be disjoint",
        ));
    }
    let service = update::Control::open(&options.name, true)?;
    service.stopped()?;
    let before = service.configuration()?;
    let old_program = program(&legacy, &before.binary_path)?;
    let profile = Profile::acquire(&config, &options.name)?;
    check_command(
        &before,
        &options.name,
        &old_program,
        &config,
        &profile.config,
    )?;
    let binary = bytes(&mut File::open(std::env::current_exe()?)?, MAX_BINARY_BYTES)?;
    crate::native_install::check_executable(&binary)?;
    let version = env!("CARGO_PKG_VERSION");
    let record = update::Record::new(
        options.name,
        config,
        profile.config.gateway_url.clone(),
        version,
        &binary,
    )?;
    let backup = Backup {
        schema: 1,
        legacy,
        native: record.clone(),
        before,
        files: old_program.proofs.clone(),
        profile: profile.proof.clone(),
    };
    let backup_bytes = serde_json::to_vec(&backup)?;
    if backup_bytes.len() as u64 > LIMIT {
        return Err(invalid("migration record exceeds the size limit"));
    }
    let mut privilege = Privilege::owner_assignment()?;
    let mut creation = Creation::default();
    let root = creation.root(&prefix, true)?;
    let versions = creation.directory(root, "versions", true)?;
    let release = creation.directory(versions, version, true)?;
    let executable = creation.file(release, "rdh.exe", &binary, true)?;
    creation.file(root, INTENT, &serde_json::to_vec(&Intent::Apply)?, true)?;
    creation.file(root, BACKUP, &backup_bytes, true)?;
    creation.file(root, update::LOCK, b"", true)?;
    creation.file(root, update::MANIFEST, &serde_json::to_vec(&record)?, true)?;
    privilege.restore()?;
    drop(privilege);
    creation.pin_directories_for_execution()?;
    // A complete preparation is retained for explicit recovery. Before this
    // point no SCM mutation has been attempted, so exact creation cleanup is safe.
    creation.keep = true;
    let _lock = update::manager_lock(&prefix, true)?;
    let _binary = update::verify_binary(&prefix, &record.current)?;
    crate::processes::check_native_program(
        &executable,
        &["--version"],
        version,
        false,
        Duration::from_secs(15),
    )?;
    profile.check()?;
    service.replace(&backup.before, &backup.after(&prefix, &record)?)?;
    profile.check()?;
    update::remove_record(&prefix, INTENT, &Intent::Apply)?;
    println!(
        "Native service migration completed. Start the service locally and verify its Hub connection. Original program, profile, pending approvals and SCM settings were retained."
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_the_old_installers_literal_argument_encoding() {
        for words in [
            vec![
                r"C:\Program Files\rdh\shawl.exe",
                "run",
                "space 😀",
                r"trailing\",
            ],
            vec!["", "embedded\"quote", r#"slash\"quote"#],
        ] {
            let command = words
                .iter()
                .map(|s| crate::processes::windows_argument(s))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(arguments(&command).unwrap(), words);
        }
        for bad in [
            "unquoted",
            "\"unterminated",
            "\"a\"\"b\"",
            "\"a\"  \"b\"",
            "\"a\" ",
            "\"a\"\n\"b\"",
        ] {
            assert!(arguments(bad).is_err(), "{bad}");
        }
    }
}
