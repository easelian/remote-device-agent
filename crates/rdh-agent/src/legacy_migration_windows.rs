//! Stopped, current-user Task Scheduler handoff. The whole task action is
//! published in one COM update; all other task properties stay fingerprinted.
use crate::{
    cli::{Action, parse},
    config::{DeviceConfig, local_roots, same_hub, sha256_hex},
    error::{Error, Result},
    install_lock::{ManagerLock, current_installation, pointer, version_binary},
    profile_lock::ProfileLocks,
    startup::{self, Binding, Mode},
    startup_native::windows::{
        startup_path,
        tasks::{self, MigrationAction},
    },
    storage,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const LIMIT: u64 = 131072;
const RUNNER_SHA256: &str = "2200d65d6c15c95456f7fbb0f94911b384458473349e93c62c3c7306fa7dedd1";
const PS_ARGUMENTS: &str =
    "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand ";
const USAGE: &str = "rdh migrate --offline|--undo --prefix OLD_INSTALL --config ABSOLUTE_PATH --service-unit TASK_NAME [--recover-lock]\nRun from a prepared native user installation after stopping the old task and all Agent work.\nThe original task settings, working directory, configuration and legacy program are retained.\nRepeat the same command after an interruption; --undo restores the original task action.\nThese commands never start, stop, enable or disable the task.\n";

fn invalid(message: &str) -> Error {
    Error::message(format!("Windows task migration refused: {message}"))
}
fn task_error(error: windows::core::Error) -> Error {
    invalid(&format!(
        "Task Scheduler HRESULT=0x{:08x}",
        error.code().0 as u32
    ))
}
fn text(path: &Path) -> Result<String> {
    let value = startup_path(path)?;
    if !path.is_absolute() || value.len() > 32000 || value.chars().any(char::is_control) {
        return Err(invalid(
            "use absolute Unicode paths without control characters",
        ));
    }
    Ok(value)
}
fn same_path(value: &str, path: &Path) -> Result<bool> {
    Ok(text(Path::new(value))?.eq_ignore_ascii_case(&text(path)?))
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

struct Held {
    file: File,
    _parents: Vec<File>,
}
impl Held {
    fn read(&mut self, limit: u64) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.file)
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(invalid("legacy file exceeds its size limit"));
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    path: PathBuf,
    sha256: String,
    bytes: u64,
    private: bool,
}
impl Proof {
    fn inspect(path: &Path, limit: u64, private: bool) -> Result<(Self, Held)> {
        use storage::windows::{
            check_file_acl, check_install_parent, open_child, open_install_parent, open_parent,
        };
        text(path)?;
        let (parents, name) = if private {
            open_parent(path, false, true)?
        } else {
            open_install_parent(path)?
        };
        let mut file = open_child(parents.last().unwrap(), &name, false, true)?;
        if private {
            check_file_acl(&file)?;
        } else {
            check_install_parent(&file)?;
        }
        let size = file.metadata()?.len();
        if size > limit {
            return Err(invalid("legacy file exceeds its size limit"));
        }
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        let mut read = 0;
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            read += n as u64;
            if read > limit {
                return Err(invalid("legacy file exceeds its size limit"));
            }
            digest.update(&buffer[..n]);
        }
        if read != size || file.metadata()?.len() != size {
            return Err(invalid("legacy file changed while reading"));
        }
        // These handles deny data writes/deletion of the file and pin every
        // ancestor through the task update. Reparse points and links are refused.
        Ok((
            Self {
                path: path.into(),
                sha256: format!("{:x}", digest.finalize()),
                bytes: size,
                private,
            },
            Held {
                file,
                _parents: parents,
            },
        ))
    }
    fn hold(&self) -> Result<Held> {
        let (observed, held) = Self::inspect(&self.path, self.bytes, self.private)?;
        if observed != *self {
            return Err(invalid("original program or profile changed"));
        }
        Ok(held)
    }
}

#[derive(Debug)]
struct LegacyArguments {
    runner: String,
    node: String,
    log: String,
    args: Vec<String>,
}
struct Literal<'a>(&'a str);
impl Literal<'_> {
    fn consume(&mut self, expected: &str) -> Result<()> {
        self.0 = self
            .0
            .strip_prefix(expected)
            .ok_or_else(|| invalid("unrecognized literal task command"))?;
        Ok(())
    }
    fn string(&mut self) -> Result<String> {
        self.consume("'")?;
        let mut value = String::new();
        loop {
            if self.0.starts_with("''") {
                value.push('\'');
                self.0 = &self.0[2..];
            } else if self.0.starts_with('\'') {
                self.0 = &self.0[1..];
                if value.chars().any(char::is_control) {
                    return Err(invalid("control character in task argument"));
                }
                return Ok(value);
            } else {
                let c = self
                    .0
                    .chars()
                    .next()
                    .ok_or_else(|| invalid("unterminated task argument"))?;
                value.push(c);
                self.0 = &self.0[c.len_utf8()..];
            }
        }
    }
}
fn decode(arguments: &str) -> Result<LegacyArguments> {
    let encoded = arguments
        .strip_prefix(PS_ARGUMENTS)
        .ok_or_else(|| invalid("unrecognized PowerShell arguments"))?;
    if encoded.len() as u64 > LIMIT {
        return Err(invalid("task command is too large"));
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| invalid("invalid encoded task command"))?;
    if bytes.len() % 2 != 0 {
        return Err(invalid("invalid UTF-16 task command"));
    }
    let units: Vec<_> = bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    let command = String::from_utf16(&units).map_err(|_| invalid("invalid UTF-16 task command"))?;
    let mut p = Literal(&command);
    p.consume("& ")?;
    let runner = p.string()?;
    p.consume(" -Node ")?;
    let node = p.string()?;
    p.consume(" -Log ")?;
    let log = p.string()?;
    p.consume(" -AgentArguments @(")?;
    let mut args = Vec::new();
    loop {
        if args.len() >= 64 {
            return Err(invalid("too many legacy task arguments"));
        }
        args.push(p.string()?);
        if p.0 == ")" {
            break;
        }
        p.consume(",")?;
    }
    Ok(LegacyArguments {
        runner,
        node,
        log,
        args,
    })
}

struct Options {
    undo: bool,
    legacy: PathBuf,
    config: PathBuf,
    name: String,
    recover: bool,
}
fn options(args: &[String]) -> Result<Options> {
    let (mut operation, mut prefix, mut config, mut name, mut recover) =
        (None, None, None, None, false);
    let mut items = args.iter();
    while let Some(flag) = items.next() {
        match flag.as_str() {
            "--offline" | "--undo" if operation.is_none() => operation = Some(flag == "--undo"),
            "--recover-lock" if !recover => recover = true,
            "--prefix" | "--config" | "--service-unit" => {
                let value = items
                    .next()
                    .filter(|v| !v.is_empty() && !v.starts_with("--"))
                    .ok_or_else(|| Error::message(USAGE))?;
                let slot = match flag.as_str() {
                    "--prefix" => &mut prefix,
                    "--config" => &mut config,
                    _ => &mut name,
                };
                if slot.replace(value.clone()).is_some() {
                    return Err(Error::message(USAGE));
                }
            }
            _ => return Err(Error::message(USAGE)),
        }
    }
    let (Some(undo), Some(prefix), Some(config), Some(name)) = (operation, prefix, config, name)
    else {
        return Err(Error::message(USAGE));
    };
    if !startup::valid_name(&name) {
        return Err(Error::message(USAGE));
    }
    text(Path::new(&prefix))?;
    text(Path::new(&config))?;
    // Verify the original spellings before canonicalizing; an alias must not
    // hide a junction or a private-profile ownership mismatch.
    let _prefix =
        storage::windows::open_install_parent(&Path::new(&prefix).join(".migration-inspection"))?;
    let _config = Proof::inspect(Path::new(&config), 1024 * 1024, true)?;
    Ok(Options {
        undo,
        legacy: Path::new(&prefix).canonicalize()?,
        config: Path::new(&config).canonicalize()?,
        name,
        recover,
    })
}
fn binding(options: &Options) -> Result<()> {
    if startup::inspect_sidecar(&options.config)?.0
        != Some(Binding {
            mode: Mode::User,
            name: options.name.clone(),
        })
    {
        return Err(invalid("profile is not bound to this user task"));
    }
    Ok(())
}
fn native_action(config: &Path, binary: &Path, directory: &str) -> Result<MigrationAction> {
    let config_text = text(config)?;
    let paired = DeviceConfig::read(config)?;
    text(Path::new(directory))?;
    Ok(MigrationAction {
        executable: text(binary)?,
        directory: directory.into(),
        arguments: [
            "--config",
            &config_text,
            "--server",
            &paired.gateway_url,
            "--no-browser",
            "--recover-lock",
        ]
        .into_iter()
        .map(crate::processes::windows_argument)
        .collect::<Vec<_>>()
        .join(" "),
    })
}
fn check_launch(parsed: &LegacyArguments, config: &Path, directory: &str) -> Result<()> {
    let args = parsed
        .args
        .get(1..)
        .ok_or_else(|| invalid("missing legacy entry"))?;
    let Action::Launch(launch) = parse(args, None)? else {
        return Err(invalid("unsupported legacy operation"));
    };
    let paired = DeviceConfig::read(config)?;
    if !same_path(
        launch
            .config
            .as_deref()
            .ok_or_else(|| invalid("legacy config is missing"))?,
        config,
    )? || launch.pair_only
        || launch.name.is_some()
        || !same_hub(&launch.server, &paired.gateway_url)
        || args.iter().filter(|s| s.as_str() == "--config").count() != 1
        || args.iter().filter(|s| s.as_str() == "--server").count() > 1
    {
        return Err(invalid("legacy arguments differ from the paired profile"));
    }
    if !launch.roots.is_empty() {
        let roots = launch
            .roots
            .iter()
            .map(|s| {
                let (name, path) = s
                    .split_once('=')
                    .ok_or_else(|| invalid("invalid legacy root"))?;
                let path = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    Path::new(directory).join(path)
                };
                Ok(format!("{name}={}", text(&path)?))
            })
            .collect::<Result<Vec<_>>>()?;
        if local_roots(&roots)? != paired.roots {
            return Err(invalid("legacy roots differ from the paired profile"));
        }
    }
    Ok(())
}

fn check_old_action(options: &Options, old: &MigrationAction, directory: &Path) -> Result<()> {
    let parsed = decode(&old.arguments)?;
    let powershell = PathBuf::from(
        std::env::var_os("SystemRoot").ok_or_else(|| invalid("SystemRoot is missing"))?,
    )
    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    if !same_path(&old.executable, &powershell)?
        || !same_path(&parsed.node, &directory.join("node.exe"))?
        || !same_path(&parsed.runner, &directory.join("windows-logon.ps1"))?
        || !same_path(
            parsed
                .args
                .first()
                .ok_or_else(|| invalid("missing legacy entry"))?,
            &directory.join("bin/remote-device-hub.mjs"),
        )?
    {
        return Err(invalid("unrecognized legacy package and task wrapper"));
    }
    let installer = same_path(&old.directory, &options.legacy)?
        && same_path(&parsed.log, &options.legacy.join("data/agent.log"))?
        && options.config == options.legacy.join("data/agent.json");
    let profile = same_path(&old.directory, options.config.parent().unwrap())?
        && same_path(
            &parsed.log,
            &PathBuf::from(format!("{}.startup.log", text(&options.config)?)),
        )?;
    if !installer && !profile {
        return Err(invalid(
            "legacy working directory or log layout is not recognized",
        ));
    }
    check_launch(&parsed, &options.config, &old.directory)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u8,
    legacy: PathBuf,
    native: PathBuf,
    config: PathBuf,
    name: String,
    sid: String,
    version: String,
    program: Vec<Proof>,
    profile: Vec<Proof>,
    policy_present: bool,
    old: MigrationAction,
    new: MigrationAction,
    fingerprint: String,
}
impl Record {
    fn prepare(options: &Options, native: &Path, binary: &Path, sid: &str) -> Result<Self> {
        let (old, fingerprint) =
            tasks::migration_snapshot(&options.name, sid).map_err(task_error)?;
        let parsed = decode(&old.arguments)?;
        let (node, mut node_file) =
            Proof::inspect(Path::new(&parsed.node), 256 * 1024 * 1024, false)?;
        // Validate the real PE file without executing the old program.
        node_file.file.seek(SeekFrom::Start(0))?;
        let mut header = vec![0; 4096.min(node.bytes as usize)];
        node_file.file.read_exact(&mut header)?;
        crate::native_install::check_executable(&header)?;
        let directory = Path::new(&parsed.node)
            .parent()
            .ok_or_else(|| invalid("legacy Node directory is missing"))?
            .canonicalize()?;
        let versions = options.legacy.join("versions");
        if directory.parent() != Some(versions.as_path())
            || !same_path(&parsed.node, &directory.join("node.exe"))?
        {
            return Err(invalid("legacy Node is outside the selected installation"));
        }
        let (package, mut package_file) =
            Proof::inspect(&directory.join("package.json"), LIMIT, false)?;
        let metadata: serde_json::Value = serde_json::from_slice(&package_file.read(LIMIT)?)?;
        let version = metadata["version"]
            .as_str()
            .filter(|v| crate::update::valid_version(v))
            .ok_or_else(|| invalid("invalid legacy version"))?;
        let architecture = if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            "arm64"
        };
        if metadata["name"] != "remote-device-hub"
            || directory.file_name().and_then(|s| s.to_str())
                != Some(&format!("rdh-{version}-win32-{architecture}"))
        {
            return Err(invalid("unrecognized legacy package and task wrapper"));
        }
        let runner = Proof::inspect(&directory.join("windows-logon.ps1"), LIMIT, false)?.0;
        if runner.sha256 != RUNNER_SHA256 {
            return Err(invalid("legacy task wrapper is not a recognized version"));
        }
        check_old_action(options, &old, &directory)?;
        let mut program = vec![node, package, runner];
        for (name, limit) in [
            ("bin/remote-device-hub.mjs", 1024 * 1024),
            ("bundle/agent.mjs", 64 * 1024 * 1024),
        ] {
            program.push(Proof::inspect(&directory.join(name), limit, false)?.0);
        }
        let mut profile = vec![
            Proof::inspect(&options.config, 1024 * 1024, true)?.0,
            Proof::inspect(&startup::sidecar_path(&options.config), 1024 * 1024, true)?.0,
        ];
        let policy = crate::policy::sidecar_path(&options.config);
        let policy_present = exists(&policy)?;
        if policy_present {
            profile.push(Proof::inspect(&policy, 1024 * 1024, true)?.0);
        }
        Ok(Self {
            schema: 1,
            legacy: options.legacy.clone(),
            native: native.into(),
            config: options.config.clone(),
            name: options.name.clone(),
            sid: sid.into(),
            version: version.into(),
            program,
            profile,
            policy_present,
            new: native_action(&options.config, binary, &old.directory)?,
            old,
            fingerprint,
        })
    }
    fn hold(
        &self,
        options: &Options,
        native: &Path,
        binary: &Path,
        sid: &str,
    ) -> Result<Vec<Held>> {
        if self.schema != 1
            || self.legacy != options.legacy
            || self.native != native
            || self.config != options.config
            || self.name != options.name
            || self.sid != sid
            || !crate::update::valid_version(&self.version)
            || self.new != native_action(&options.config, binary, &self.old.directory)?
            || self.fingerprint.len() != 64
            || !self.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            || self.program.len() != 5
        {
            return Err(invalid("migration record does not match this installation"));
        }
        let arch = if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            "arm64"
        };
        let directory = self
            .legacy
            .join("versions")
            .join(format!("rdh-{}-win32-{arch}", self.version));
        let mut held = Vec::new();
        for (proof, (name, limit)) in self.program.iter().zip([
            ("node.exe", 256 * 1024 * 1024),
            ("package.json", LIMIT),
            ("windows-logon.ps1", LIMIT),
            ("bin/remote-device-hub.mjs", 1024 * 1024),
            ("bundle/agent.mjs", 64 * 1024 * 1024),
        ]) {
            if proof.private
                || proof.bytes > limit
                || !same_path(&text(&proof.path)?, &directory.join(name))?
            {
                return Err(invalid("invalid legacy program record"));
            }
            held.push(proof.hold()?);
        }
        if self.program[2].sha256 != RUNNER_SHA256 {
            return Err(invalid("invalid legacy wrapper record"));
        }
        let metadata: serde_json::Value = serde_json::from_slice(&held[1].read(LIMIT)?)?;
        if metadata["name"] != "remote-device-hub" || metadata["version"] != self.version {
            return Err(invalid("legacy package version changed"));
        }
        check_old_action(options, &self.old, &directory)?;
        let mut profiles = vec![
            options.config.clone(),
            startup::sidecar_path(&options.config),
        ];
        let policy = crate::policy::sidecar_path(&options.config);
        if exists(&policy)? != self.policy_present {
            return Err(invalid("profile policy changed"));
        }
        if self.policy_present {
            profiles.push(policy);
        }
        if self.profile.len() != profiles.len() {
            return Err(invalid("invalid profile record"));
        }
        for (proof, path) in self.profile.iter().zip(profiles) {
            if !proof.private || proof.bytes > 1024 * 1024 || proof.path != path {
                return Err(invalid("invalid profile record path"));
            }
            held.push(proof.hold()?);
        }
        binding(options)?;
        let (action, fingerprint) =
            tasks::migration_snapshot(&self.name, sid).map_err(task_error)?;
        if fingerprint != self.fingerprint || (action != self.old && action != self.new) {
            return Err(invalid("task changed outside this migration"));
        }
        Ok(held)
    }
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    if matches!(args,[help] if help=="--help" || help=="-h") {
        print!("{USAGE}");
        return Ok(());
    }
    let options = options(args)?;
    let (native, executable) = current_installation()?
        .ok_or_else(|| invalid("run from a prepared native user installation"))?;
    if native.starts_with(&options.legacy) || options.legacy.starts_with(&native) {
        return Err(invalid("native and legacy installations must be separate"));
    }
    let sid = storage::windows::current_sid_text()?;
    tasks::migration_snapshot(&options.name, &sid).map_err(task_error)?;
    let manager =
        ManagerLock::acquire_with_stale_recovery(&native, &executable, false, options.recover)?;
    if !manager.profiles()?.contains(&options.config) {
        return Err(invalid(
            "native installation has not registered this profile",
        ));
    }
    let version = pointer(&native, "current")?;
    crate::native_install::verify_stored(&native, &version)?;
    let binary = version_binary(&native, &version)?;
    tasks::migration_snapshot(&options.name, &sid).map_err(task_error)?;
    binding(&options)?;
    let targets = [
        options.config.clone(),
        startup::sidecar_path(&options.config),
    ];
    let mut locks = ProfileLocks::acquire(&targets)?;
    crate::app::validate_config(&["--config".into(), text(&options.config)?])?;
    let record_path = native.join(format!(
        ".legacy-task-{}.json",
        sha256_hex(format!("{}\0{}", options.name, text(&options.config)?).as_bytes())
    ));
    let record: Record = if exists(&record_path)? {
        serde_json::from_slice(&storage::read_private(&record_path)?)?
    } else {
        if options.undo {
            return Err(invalid("this installation has no migration to undo"));
        }
        let record = Record::prepare(&options, &native, &binary, &sid)?;
        let _held = record.hold(&options, &native, &binary, &sid)?;
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() as u64 > LIMIT {
            return Err(invalid("migration record exceeds its size limit"));
        }
        storage::write_private_new(&record_path, &bytes)?;
        record
    };
    let _held = record.hold(&options, &native, &binary, &sid)?;
    tasks::migrate(
        &options.name,
        &sid,
        &record.fingerprint,
        &record.old,
        &record.new,
        options.undo,
    )
    .map_err(task_error)?;
    locks.release()?;
    println!(
        "{} task action. Original program and profile retained; start the task locally and verify its Hub connection.",
        if options.undo {
            "Restored legacy"
        } else {
            "Selected native"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(command: &str) -> String {
        format!(
            "{PS_ARGUMENTS}{}",
            STANDARD.encode(
                command
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            )
        )
    }
    #[test]
    fn decodes_only_the_known_literal_powershell_grammar() {
        let value=decode(&encoded("& 'C:\\old ''😀\\windows-logon.ps1' -Node 'C:\\old ''😀\\node.exe' -Log 'C:\\logs\\agent.log' -AgentArguments @('C:\\old\\entry.mjs','--config','C:\\data\\a.json')")).unwrap();
        assert_eq!(value.runner, "C:\\old '😀\\windows-logon.ps1");
        assert_eq!(value.args.len(), 3);
        for command in [
            "& 'a' -Node 'b' -Log 'c' -AgentArguments @('d'); exit 0",
            "& 'a' -Node 'b' -Log 'c' -AgentArguments @($(Get-Content x))",
            "& \"a\" -Node 'b' -Log 'c' -AgentArguments @('d')",
            "& 'a' -Node 'b' -Log 'c' -AgentArguments @('d' + 'e')",
            "& 'a' -Node 'b' -Log 'c' -AgentArguments @('d\n')",
        ] {
            assert!(decode(&encoded(command)).is_err(), "{command}");
        }
        assert!(decode(&format!("{PS_ARGUMENTS}{}", STANDARD.encode([0xd8, 0xd8]))).is_err());
        assert!(decode(&format!("{PS_ARGUMENTS}{}", STANDARD.encode([0]))).is_err());
    }
}
