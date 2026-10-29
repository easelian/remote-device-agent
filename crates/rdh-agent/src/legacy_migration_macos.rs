//! A stopped user LaunchAgent keeps its original plist, environment and startup
//! setting. Its recognized literal wrapper is the sole atomic switch point.
use crate::{
    cli::{Action, parse},
    config::{DeviceConfig, home_directory, local_roots, same_hub, sha256_hex},
    error::{Error, Result},
    install_lock::{ManagerLock, current_installation, pointer},
    legacy_shell::{WrapperWords, wrapper_words},
    profile_lock::ProfileLocks,
    startup::{self, Binding, Mode},
    storage,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    time::Duration,
};

const LIMIT: u64 = 65536;
const USAGE: &str = "rdh migrate --offline|--undo --prefix OLD_INSTALL --config ABSOLUTE_PATH --service-unit LABEL [--recover-lock]\nRun from a prepared native user installation. Boot out the legacy LaunchAgent first.\nThe original plist, startup setting, configuration and legacy program are retained.\nRepeat the same command after an interruption; --undo restores the original wrapper.\nThese commands never start, stop, enable or disable a LaunchAgent.\n";

fn invalid(message: &str) -> Error {
    Error::message(format!("LaunchAgent migration refused: {message}"))
}
fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .filter(|s| {
            path.is_absolute()
                && s.len() <= 4096
                && !s.chars().any(char::is_control)
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        })
        .ok_or_else(|| {
            invalid("paths must be absolute Unicode without control or parent components")
        })
}
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn protected_parent(path: &Path) -> Result<()> {
    text(path)?;
    let uid = unsafe { libc::getuid() };
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        let m = fs::symlink_metadata(&current)?;
        if !m.is_dir()
            || ![0, uid].contains(&m.uid())
            || (m.mode() & 0o022 != 0 && !(m.uid() == 0 && m.mode() & 0o1000 != 0))
        {
            return Err(invalid(
                "legacy paths need protected directories without symlinks",
            ));
        }
        storage::macos_acl::check(&current)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Proof {
    path: PathBuf,
    sha256: String,
    bytes: u64,
    mode: u32,
    gid: u32,
}
impl Proof {
    fn inspect(path: &Path, limit: u64) -> Result<(Self, Vec<u8>)> {
        text(path)?;
        protected_parent(
            path.parent()
                .ok_or_else(|| invalid("file parent is missing"))?,
        )?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let before = file.metadata()?;
        storage::macos_acl::check(path)?;
        if !before.is_file()
            || before.uid() != unsafe { libc::getuid() }
            || before.mode() & 0o7022 != 0
            || before.nlink() != 1
            || before.len() > limit
        {
            return Err(invalid(
                "legacy files must be owned by this user, protected and bounded",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        let named = fs::symlink_metadata(path)?;
        if bytes.len() as u64 != before.len()
            || bytes.len() as u64 > limit
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
            || after.dev() != named.dev()
            || after.ino() != named.ino()
        {
            return Err(invalid("legacy file changed during inspection"));
        }
        Ok((
            Self {
                path: path.into(),
                sha256: sha256_hex(&bytes),
                bytes: bytes.len() as u64,
                mode: before.mode() & 0o777,
                gid: before.gid(),
            },
            bytes,
        ))
    }
    fn recheck(&self) -> Result<()> {
        if Self::inspect(&self.path, self.bytes)?.0 != *self {
            return Err(invalid(
                "original files changed since migration preparation",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u8,
    legacy: PathBuf,
    native: PathBuf,
    config: PathBuf,
    label: String,
    version: String,
    program: Vec<Proof>,
    profile: Vec<Proof>,
    policy_present: bool,
    plist: Proof,
    wrapper: Proof,
    old: String,
    new: String,
}

struct Options {
    undo: bool,
    legacy: PathBuf,
    config: PathBuf,
    label: String,
    recover: bool,
}
fn options(args: &[String]) -> Result<Options> {
    let (mut operation, mut prefix, mut config, mut label, mut recover) =
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
                    _ => &mut label,
                };
                if slot.replace(value.clone()).is_some() {
                    return Err(Error::message(USAGE));
                }
            }
            _ => return Err(Error::message(USAGE)),
        }
    }
    let (Some(undo), Some(prefix), Some(config), Some(label)) = (operation, prefix, config, label)
    else {
        return Err(Error::message(USAGE));
    };
    if !startup::valid_name(&label) {
        return Err(Error::message(USAGE));
    }
    for p in [&prefix, &config] {
        text(Path::new(p))?;
    }
    Ok(Options {
        undo,
        legacy: prefix.into(),
        config: config.into(),
        label,
        recover,
    })
}
fn stopped(label: &str) -> Result<()> {
    let target = format!("gui/{}/{label}", unsafe { libc::getuid() });
    let mut command = std::process::Command::new("/bin/launchctl");
    command.args(["print", &target]);
    let (status, _) = crate::startup_native::posix::run_command(command, Duration::from_secs(12))?;
    if status.code() != Some(113) {
        return Err(invalid("boot out the user LaunchAgent before migration"));
    }
    Ok(())
}
fn version(prefix: &Path) -> Result<String> {
    protected_parent(prefix)?;
    let current = prefix.join("current");
    let m = fs::symlink_metadata(&current)?;
    if !m.file_type().is_symlink() || m.uid() != unsafe { libc::getuid() } {
        return Err(invalid("legacy current must be a user-owned symlink"));
    }
    let link = fs::read_link(current)?;
    let mut parts = link.components();
    let (Some(Component::Normal(versions)), Some(Component::Normal(v)), None) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("unrecognized legacy current pointer"));
    };
    let v = v
        .to_str()
        .filter(|v| crate::update::valid_version(v))
        .ok_or_else(|| invalid("invalid legacy version"))?;
    if versions != "versions" {
        return Err(invalid("invalid legacy current pointer"));
    }
    Ok(v.into())
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn xml(s: &str, installer: bool) -> String {
    let s = s
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if installer {
        s.replace('"', "&quot;")
    } else {
        s
    }
}
fn definition(
    label: &str,
    wrapper: &Path,
    words: &WrapperWords,
    installer: bool,
) -> Result<String> {
    let wrapper = xml(text(wrapper)?, installer);
    Ok(if installer {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>{wrapper}</string></array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict></plist>\n",
            xml(&words.working_directory, true)
        )
    } else {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{wrapper}</string></array><key>RunAtLoad</key><true/></dict></plist>\n"
        )
    })
}
fn converted(old: &str, parsed: &WrapperWords, offset: usize, native: &Path) -> Result<String> {
    let head = old
        .rsplit_once("\nexec ")
        .ok_or_else(|| invalid("invalid wrapper"))?
        .0;
    let mut args = vec![text(&native.join("current/rdh"))?.to_owned()];
    args.extend_from_slice(
        parsed
            .words
            .get(offset..)
            .ok_or_else(|| invalid("incomplete wrapper arguments"))?,
    );
    Ok(format!(
        "{head}\nexec {}\n",
        args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
    ))
}
fn check_launch(
    parsed: &WrapperWords,
    offset: usize,
    config: &Path,
    paired: &DeviceConfig,
) -> Result<()> {
    let arguments = &parsed.words[offset..];
    let Action::Launch(launch) = parse(arguments, None)? else {
        return Err(invalid("unexpected legacy command"));
    };
    if launch.config.as_deref() != Some(text(config)?)
        || launch.pair_only
        || launch.name.is_some()
        || !same_hub(&launch.server, &paired.gateway_url)
        || arguments
            .iter()
            .filter(|a| a.as_str() == "--config")
            .count()
            != 1
        || arguments
            .iter()
            .filter(|a| a.as_str() == "--server")
            .count()
            > 1
    {
        return Err(invalid("legacy arguments do not match the paired profile"));
    }
    if !launch.roots.is_empty() {
        let roots = launch
            .roots
            .iter()
            .map(|root| {
                let (name, path) = root
                    .split_once('=')
                    .ok_or_else(|| invalid("invalid legacy root"))?;
                let path = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    Path::new(&parsed.working_directory).join(path)
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
fn binding(config: &Path, label: &str) -> Result<()> {
    if startup::inspect_sidecar(config)?.0
        != Some(Binding {
            mode: Mode::User,
            name: label.into(),
        })
    {
        return Err(invalid(
            "configuration is not bound to this user LaunchAgent",
        ));
    }
    Ok(())
}
fn check_program(
    parsed: &WrapperWords,
    installer: bool,
    prefix: &Path,
    version: &str,
) -> Result<usize> {
    if installer {
        if parsed.words.first().map(String::as_str) != Some(text(&prefix.join("current/rdh"))?) {
            return Err(invalid("legacy wrapper selects another installation"));
        }
        Ok(1)
    } else {
        let directory = prefix.join("versions").join(version);
        if parsed.words.len() < 3
            || ["node", "bin/remote-device-hub.mjs"]
                .iter()
                .enumerate()
                .any(|(i, file)| {
                    let path = Path::new(&parsed.words[i]);
                    path != directory.join(file) && path != prefix.join("current").join(file)
                })
        {
            return Err(invalid("legacy profile wrapper selects another program"));
        }
        Ok(2)
    }
}
impl Record {
    fn prepare(options: &Options, native: &Path, plist: &Path) -> Result<Self> {
        let version = version(&options.legacy)?;
        let directory = options.legacy.join("versions").join(&version);
        let mut program = Vec::new();
        for (file, limit) in [
            ("node", 256 * 1024 * 1024),
            ("rdh", LIMIT),
            ("package.json", LIMIT),
            ("bin/remote-device-hub.mjs", 1024 * 1024),
            ("bundle/agent.mjs", 64 * 1024 * 1024),
        ] {
            program.push(Proof::inspect(&directory.join(file), limit)?.0);
        }
        let package: serde_json::Value =
            serde_json::from_slice(&Proof::inspect(&directory.join("package.json"), LIMIT)?.1)?;
        if package["name"] != "remote-device-hub"
            || package["version"] != version
            || program[0].mode & 0o100 == 0
            || program[1].mode & 0o100 == 0
            || program[1].sha256
                != "8e045d68f8d5fadadc890782c40c36abb065754bfbdc17fcd749aed166df5765"
        {
            return Err(invalid("legacy program layout is not recognized"));
        }
        crate::native_install::check_executable(&directory.join("node"))?;
        let (plist_proof, plist_bytes) = Proof::inspect(plist, LIMIT)?;
        let paired = DeviceConfig::read(&options.config)?;
        let mut wrapper = None;
        for (path, installer) in [
            (options.legacy.join("run-agent"), true),
            (
                PathBuf::from(format!("{}.startup-run", text(&options.config)?)),
                false,
            ),
        ] {
            if !exists(&path)? {
                continue;
            }
            let (proof, bytes) = Proof::inspect(&path, LIMIT)?;
            let Some(parsed) = wrapper_words(&bytes, installer, &options.config) else {
                continue;
            };
            if definition(&options.label, &path, &parsed, installer)?.as_bytes() != plist_bytes {
                continue;
            }
            if proof.mode & 0o100 == 0 {
                return Err(invalid("legacy wrapper is not executable"));
            }
            let offset = check_program(&parsed, installer, &options.legacy, &version)?;
            for (_, value) in &parsed.environment {
                text(Path::new(value))?;
            }
            check_launch(&parsed, offset, &options.config, &paired)?;
            let old = String::from_utf8(bytes).map_err(|_| invalid("wrapper is not Unicode"))?;
            let new = converted(&old, &parsed, offset, native)?;
            wrapper = Some((proof, old, new));
            break;
        }
        let (wrapper, old, new) = wrapper
            .ok_or_else(|| invalid("plist and wrapper do not match a supported legacy format"))?;
        let mut profile = vec![
            Proof::inspect(&options.config, 1024 * 1024)?.0,
            Proof::inspect(&startup::sidecar_path(&options.config), 1024 * 1024)?.0,
        ];
        let policy = crate::policy::sidecar_path(&options.config);
        let policy_present = exists(&policy)?;
        if policy_present {
            profile.push(Proof::inspect(&policy, 1024 * 1024)?.0);
        }
        Ok(Self {
            schema: 1,
            legacy: options.legacy.clone(),
            native: native.into(),
            config: options.config.clone(),
            label: options.label.clone(),
            version,
            program,
            profile,
            policy_present,
            plist: plist_proof,
            wrapper,
            old,
            new,
        })
    }
    fn recheck(&self, options: &Options, native: &Path, plist: &Path) -> Result<(Proof, Vec<u8>)> {
        if self.schema != 1
            || self.legacy != options.legacy
            || self.native != native
            || self.config != options.config
            || self.label != options.label
            || self.plist.path != plist
            || version(&self.legacy)? != self.version
            || self.old.len() as u64 > LIMIT
            || self.new.len() as u64 > LIMIT
            || self.wrapper.sha256 != sha256_hex(self.old.as_bytes())
            || self.wrapper.bytes != self.old.len() as u64
        {
            return Err(invalid("migration record does not match this installation"));
        }
        let directory = self.legacy.join("versions").join(&self.version);
        let files = [
            "node",
            "rdh",
            "package.json",
            "bin/remote-device-hub.mjs",
            "bundle/agent.mjs",
        ];
        if self.program.len() != files.len() {
            return Err(invalid("incomplete legacy program record"));
        }
        for (proof, name) in self.program.iter().zip(files) {
            if proof.path != directory.join(name) {
                return Err(invalid("invalid legacy program path"));
            }
            proof.recheck()?;
        }
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
            return Err(invalid("incomplete profile record"));
        }
        for (proof, path) in self.profile.iter().zip(profiles) {
            if proof.path != path {
                return Err(invalid("invalid profile record path"));
            }
            proof.recheck()?;
        }
        binding(&options.config, &options.label)?;
        self.plist.recheck()?;
        let installer = self.wrapper.path == self.legacy.join("run-agent");
        if !installer
            && self.wrapper.path != Path::new(&format!("{}.startup-run", text(&self.config)?))
        {
            return Err(invalid("invalid wrapper record path"));
        }
        let parsed = wrapper_words(self.old.as_bytes(), installer, &self.config)
            .ok_or_else(|| invalid("invalid original wrapper record"))?;
        let offset = check_program(&parsed, installer, &self.legacy, &self.version)?;
        check_launch(
            &parsed,
            offset,
            &self.config,
            &DeviceConfig::read(&self.config)?,
        )?;
        if converted(&self.old, &parsed, offset, native)? != self.new
            || sha256_hex(
                definition(&self.label, &self.wrapper.path, &parsed, installer)?.as_bytes(),
            ) != self.plist.sha256
        {
            return Err(invalid("invalid native wrapper record"));
        }
        let (actual, bytes) = Proof::inspect(&self.wrapper.path, LIMIT)?;
        if actual.mode != self.wrapper.mode
            || actual.gid != self.wrapper.gid
            || (bytes != self.old.as_bytes() && bytes != self.new.as_bytes())
        {
            return Err(invalid("wrapper changed outside this migration"));
        }
        Ok((actual, bytes))
    }
}

fn replace(expected: &Proof, bytes: &[u8]) -> Result<()> {
    let parent = expected
        .path
        .parent()
        .ok_or_else(|| invalid("wrapper parent is missing"))?;
    let temporary = parent.join(format!(
        ".rdh-migrate-{}",
        crate::random::hex_token::<16>()?
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        if unsafe { libc::fchown(file.as_raw_fd(), !0, expected.gid) } != 0
            || unsafe { libc::fchmod(file.as_raw_fd(), expected.mode as libc::mode_t) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        expected.recheck()?;
        fs::rename(&temporary, &expected.path)?;
        storage::sync_directory(&File::open(parent)?)?;
        Ok(())
    })();
    if result.is_err()
        && let (Ok(held), Ok(named)) = (file.metadata(), fs::symlink_metadata(&temporary))
        && held.dev() == named.dev()
        && held.ino() == named.ino()
    {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    if matches!(args, [help] if help == "--help" || help == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    let options = options(args)?;
    if unsafe { libc::getuid() } == 0 || unsafe { libc::getuid() } != unsafe { libc::geteuid() } {
        return Err(invalid("run as the ordinary user who owns the LaunchAgent"));
    }
    let (native, executable) = current_installation()?
        .ok_or_else(|| invalid("run from a prepared native installation"))?;
    if options.config.canonicalize()? != options.config
        || options.legacy.canonicalize()? != options.legacy
        || native.starts_with(&options.legacy)
        || options.legacy.starts_with(&native)
    {
        return Err(invalid(
            "use canonical profile paths and separate native and legacy installations",
        ));
    }
    let manager = ManagerLock::acquire(&native, &executable, false)?;
    if !manager.profiles()?.contains(&options.config) {
        return Err(invalid(
            "native installation has not registered this profile",
        ));
    }
    crate::native_install::verify_stored(&native, &pointer(&native, "current")?)?;
    let plist = home_directory()?
        .canonicalize()?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", options.label));
    let record_path = native.join(format!(
        ".legacy-launchagent-{}.json",
        sha256_hex(format!("{}\0{}", options.label, text(&options.config)?).as_bytes())
    ));
    let targets = [
        options.config.clone(),
        startup::sidecar_path(&options.config),
    ];
    stopped(&options.label)?;
    binding(&options.config, &options.label)?;
    if options.recover {
        ProfileLocks::recover(&targets)?;
    }
    let mut locks = ProfileLocks::acquire(&targets)?;
    crate::app::validate_config(&["--config".into(), text(&options.config)?.into()])?;
    let record: Record = if exists(&record_path)? {
        serde_json::from_slice(&storage::read_private(&record_path)?)?
    } else {
        if options.undo {
            return Err(invalid("this installation has no migration to undo"));
        }
        let record = Record::prepare(&options, &native, &plist)?;
        record.recheck(&options, &native, &plist)?;
        storage::write_private_new(&record_path, &serde_json::to_vec(&record)?)?;
        record
    };
    let (observed, bytes) = record.recheck(&options, &native, &plist)?;
    stopped(&options.label)?;
    let desired = if options.undo {
        &record.old
    } else {
        &record.new
    };
    if bytes != desired.as_bytes() {
        replace(&observed, desired.as_bytes())?;
    }
    record.recheck(&options, &native, &plist)?;
    locks.release()?;
    println!(
        "{} LaunchAgent wrapper selected. The plist, startup setting, profile and rollback files were retained. Bootstrap it locally and verify the Hub connection.",
        if options.undo { "Legacy" } else { "Native" }
    );
    Ok(())
}
