//! Offline replacement of a recognized root-owned legacy service definition.
//! The old program, wrapper, configuration and startup approval are retained.
use super::{
    INSTALL_PENDING, LOCK, PENDING, RECORD_LIMIT, Record, definition, exists, manager, path_text,
    pointer, profile::ProfileLease, read_record, registration, store, verify,
};
use crate::{
    cli::{Action, parse},
    config::same_hub,
    error::{Error, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const MIGRATION: &str = ".service-migration.json";
const INTENT: &str = ".service-migration-intent";
const UNDONE: &str = ".service-migration-undone";

fn invalid(message: &str) -> Error {
    Error::message(format!(
        "Legacy system service migration refused: {message}"
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FileProof {
    path: PathBuf,
    sha256: String,
    bytes: u64,
    mode: u32,
}
impl FileProof {
    fn inspect(path: &Path, limit: u64) -> Result<Self> {
        let mut file = store::open(path)?;
        let before = file.metadata()?;
        if before.len() > limit {
            return Err(invalid("legacy file exceeds its size limit"));
        }
        let mut hash = sha2::Sha256::new();
        use sha2::Digest;
        let mut size = 0;
        let mut buf = [0; 65536];
        loop {
            let count = file.read(&mut buf)?;
            if count == 0 {
                break;
            }
            size += count as u64;
            if size > limit {
                return Err(invalid("legacy file exceeds its size limit"));
            }
            hash.update(&buf[..count]);
        }
        let after = file.metadata()?;
        let named = store::open(path)?.metadata()?;
        if size != before.len()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
            || before.dev() != named.dev()
            || before.ino() != named.ino()
        {
            return Err(invalid("legacy file changed during inspection"));
        }
        Ok(Self {
            path: path.into(),
            sha256: format!("{:x}", hash.finalize()),
            bytes: size,
            mode: before.mode() & 0o777,
        })
    }
    fn recheck(&self) -> Result<()> {
        if self != &Self::inspect(&self.path, self.bytes)? {
            return Err(invalid("legacy rollback files changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Legacy {
    schema: u8,
    prefix: PathBuf,
    version: String,
    files: Vec<FileProof>,
    definition: String,
}

fn version(prefix: &Path) -> Result<String> {
    store::directory(prefix)?;
    let path = prefix.join("current");
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_symlink() || metadata.uid() != 0 {
        return Err(invalid(
            "legacy current pointer must be a root-owned symlink",
        ));
    }
    let target = fs::read_link(path)?;
    let mut parts = target.components();
    let (
        Some(std::path::Component::Normal(versions)),
        Some(std::path::Component::Normal(version)),
        None,
    ) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("unrecognized legacy current pointer"));
    };
    let value = version
        .to_str()
        .ok_or_else(|| invalid("invalid legacy version"))?;
    if versions != "versions" || !crate::update::valid_version(value) {
        return Err(invalid("invalid legacy version"));
    }
    Ok(value.into())
}

#[cfg(target_os = "macos")]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn legacy_definition(prefix: &Path, record: &Record) -> Result<String> {
    let wrapper = prefix.join("run-agent");
    #[cfg(target_os = "linux")]
    {
        let start = path_text(&wrapper)?
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$");
        Ok(format!(
            "[Unit]\nDescription=Remote Device Hub Agent\nAfter=network-online.target\n\n[Service]\nUser={}\nGroup={}\nExecStart=\"{start}\"\nRestart=on-failure\nRestartSec=5\nKillMode=mixed\nTimeoutStopSec=30\n\n[Install]\nWantedBy=multi-user.target\n",
            record.user, record.gid
        ))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{}</string>\n<key>ProgramArguments</key><array><string>{}</string></array>\n<key>WorkingDirectory</key><string>{}</string>\n<key>UserName</key><string>{}</string>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n</dict></plist>\n",
            xml(&record.name),
            xml(path_text(&wrapper)?),
            xml(path_text(&record.state)?),
            xml(&record.user)
        ))
    }
}

fn manager_stopped(record: &Record, legacy: bool) -> Result<()> {
    // Reload before checking so systemd's cached properties describe the
    // exact on-disk definition that will be replaced. Root editors are trusted.
    #[cfg(target_os = "linux")]
    {
        super::checked_manager(&["daemon-reload"])?;
        super::stopped(record)?;
        let (status, output) = manager(&[
            "show",
            &record.name,
            "--property=FragmentPath,User,Group,Environment,EnvironmentFiles,ControlPID",
        ])?;
        let map: std::collections::BTreeMap<_, _> =
            output.lines().filter_map(|l| l.split_once('=')).collect();
        let uid = record.uid.to_string();
        let gid = record.gid.to_string();
        if !status.success()
            || map.get("FragmentPath") != Some(&path_text(&registration(record))?)
            || map.get("ControlPID") != Some(&"0")
            || map.get("User")
                != Some(&if legacy {
                    record.user.as_str()
                } else {
                    uid.as_str()
                })
            || map.get("Group") != Some(&gid.as_str())
            || map.get("EnvironmentFiles") != Some(&"")
            || (legacy && map.get("Environment") != Some(&""))
        {
            return Err(invalid(
                "effective service account or environment is not recognized",
            ));
        }
        // Both definitions run in the same system manager and therefore keep
        // its inherited environment. The exact file check rejects unit-local
        // overrides that cannot be represented by the native definition.
    }
    #[cfg(target_os = "macos")]
    {
        // Unloading removes launchd's cached legacy program, uid and arguments.
        if manager(&["print", &format!("system/{}", record.name)])?
            .0
            .code()
            != Some(113)
        {
            return Err(invalid("boot out the LaunchDaemon before migration"));
        }
        // launchd's service environment is retained by using the same domain;
        // the exact legacy plist itself has no EnvironmentVariables overrides.
        let _ = legacy;
    }
    Ok(())
}

impl Legacy {
    pub fn inspect(prefix: &Path, record: &Record, ready: &super::profile::Ready) -> Result<Self> {
        path_text(prefix)?;
        let prefix = prefix.canonicalize()?;
        let version = version(&prefix)?;
        let directory = prefix.join("versions").join(&version);
        if exists(&prefix.join(super::RECORD))? || exists(&prefix.join(".native-install.json"))? {
            return Err(invalid("the source is already a native installation"));
        }
        let mut files = Vec::new();
        for (relative, limit) in [
            ("node", 256 * 1024 * 1024),
            ("rdh", 65536),
            ("package.json", 65536),
            ("bin/remote-device-hub.mjs", 1024 * 1024),
            ("bundle/agent.mjs", 64 * 1024 * 1024),
        ] {
            files.push(FileProof::inspect(&directory.join(relative), limit)?);
        }
        let package: serde_json::Value =
            serde_json::from_slice(&store::read(&directory.join("package.json"), 65536)?)?;
        if package["name"] != "remote-device-hub"
            || package["version"] != version
            || files[1].sha256 != "8e045d68f8d5fadadc890782c40c36abb065754bfbdc17fcd749aed166df5765"
            || files[0].mode & 0o111 != 0o111
            || files[1].mode & 0o111 != 0o111
        {
            return Err(invalid("unrecognized legacy runtime layout"));
        }
        crate::native_install::check_executable(&directory.join("node"))?;
        let wrapper = prefix.join("run-agent");
        files.push(FileProof::inspect(&wrapper, 65536)?);
        if files.last().unwrap().mode & 0o111 != 0o111 {
            return Err(invalid("legacy wrapper is not executable"));
        }
        let bytes = store::read(&wrapper, 65536)?;
        let parsed = crate::legacy_shell::wrapper_words(&bytes, true, &record.config)
            .ok_or_else(|| invalid("unrecognized legacy service wrapper"))?;
        let expected_state = path_text(&record.state)?;
        if parsed.words.first().map(String::as_str) != Some(path_text(&prefix.join("current/rdh"))?)
            || parsed.environment != vec![("XDG_STATE_HOME".into(), expected_state.into())]
            || parsed.working_directory != expected_state
        {
            return Err(invalid(
                "legacy wrapper has different program, state or working directory",
            ));
        }
        let arguments = &parsed.words[1..];
        let Action::Launch(launch) = parse(arguments, None)? else {
            return Err(invalid("legacy launch arguments are unsupported"));
        };
        if launch.config.as_deref() != Some(path_text(&record.config)?)
            || launch.pair_only
            || !same_hub(&launch.server, &record.server)
            || launch.name.is_some()
            || arguments
                .iter()
                .filter(|v| v.as_str() == "--config")
                .count()
                != 1
            || arguments
                .iter()
                .filter(|v| v.as_str() == "--server")
                .count()
                > 1
        {
            return Err(invalid("legacy arguments do not match the paired profile"));
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
                        record.state.join(path)
                    };
                    Ok(format!("{name}={}", path_text(&path)?))
                })
                .collect::<Result<Vec<_>>>()?;
            if crate::config::local_roots(&roots)? != ready.roots {
                return Err(invalid("legacy roots differ from the paired profile"));
            }
        }
        let definition = legacy_definition(&prefix, record)?;
        if store::read(&registration(record), RECORD_LIMIT)? != definition.as_bytes() {
            return Err(invalid(
                "service definition differs from the recognized installer",
            ));
        }
        manager_stopped(record, true)?;
        let result = Self {
            schema: 1,
            prefix,
            version,
            files,
            definition,
        };
        result.recheck(record)?;
        Ok(result)
    }
    fn recheck(&self, record: &Record) -> Result<()> {
        if self.schema != 1
            || self.files.len() != 6
            || version(&self.prefix)? != self.version
            || legacy_definition(&self.prefix, record)? != self.definition
        {
            return Err(invalid("invalid or changed migration record"));
        }
        let directory = self.prefix.join("versions").join(&self.version);
        for (file, expected) in self.files.iter().zip([
            directory.join("node"),
            directory.join("rdh"),
            directory.join("package.json"),
            directory.join("bin/remote-device-hub.mjs"),
            directory.join("bundle/agent.mjs"),
            self.prefix.join("run-agent"),
        ]) {
            if file.path != expected {
                return Err(invalid("invalid rollback file path"));
            }
            file.recheck()?;
        }
        Ok(())
    }
    pub fn save(&self, prefix: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > RECORD_LIMIT {
            return Err(invalid("migration record exceeds its size limit"));
        }
        store::create(&prefix.join(MIGRATION), &bytes, false)?;
        store::create(&prefix.join(INTENT), b"apply\n", false)
    }
    pub fn publish(&self, prefix: &Path, record: &Record) -> Result<()> {
        self.recheck(record)?;
        if store::read(&registration(record), RECORD_LIMIT)? != self.definition.as_bytes() {
            return Err(invalid("legacy registration changed before publication"));
        }
        manager_stopped(record, true)?;
        store::replace(&registration(record), &definition(prefix, record)?)?;
        #[cfg(target_os = "linux")]
        super::checked_manager(&["daemon-reload"])?;
        Ok(())
    }
}

pub(super) fn complete(prefix: &Path) -> Result<()> {
    // Removing the startup barrier is the last semantic commit. The durable
    // intent stays until after it, making recovery safe on either side.
    fs::remove_file(prefix.join(INSTALL_PENDING))?;
    store::sync(prefix)?;
    if exists(&prefix.join(INTENT))? {
        fs::remove_file(prefix.join(INTENT))?;
        store::sync(prefix)?;
    }
    Ok(())
}

pub(super) fn resume(args: &[String]) -> Result<bool> {
    let [operation, flag, prefix] = args else {
        return Ok(false);
    };
    if !matches!(
        operation.as_str(),
        "--recover-migration" | "--undo-migration"
    ) {
        return Ok(false);
    }
    if flag != "--prefix" {
        return Err(invalid(
            "use --recover-migration|--undo-migration --prefix NATIVE_INSTALL",
        ));
    }
    store::root()?;
    path_text(Path::new(prefix))?;
    let prefix = Path::new(prefix).canonicalize()?;
    let _lock = store::Lock::acquire(&prefix.join(LOCK), true)?;
    if exists(&prefix.join(PENDING))? {
        return Err(invalid(
            "recover the native version update before changing its service registration",
        ));
    }
    let record = read_record(&prefix)?;
    if !record.migrated || pointer(&prefix)? != record.current.version {
        return Err(invalid("invalid native migration installation"));
    }
    let legacy: Legacy =
        serde_json::from_slice(&store::read(&prefix.join(MIGRATION), RECORD_LIMIT)?)?;
    legacy.recheck(&record)?;
    let old = legacy.definition.as_bytes();
    let new = definition(&prefix, &record)?;
    let observed = store::read(&registration(&record), RECORD_LIMIT)?;
    if observed != old && observed != new {
        return Err(invalid("registration changed outside the migration"));
    }
    manager_stopped(&record, observed == old)?;
    let account = super::profile::Account::lookup(&record.user)?;
    if account.uid != record.uid || account.gid != record.gid {
        return Err(invalid("service account changed"));
    }
    let mut profile = ProfileLease::start(
        &verify(&prefix, &record.current)?,
        &account,
        &record.config,
        &record.state,
        &record.name,
        &record.current.version,
    )?;
    if !profile.ready.bound
        || profile.ready.config != record.config
        || profile.ready.state != record.state
        || profile.ready.server != record.server
    {
        return Err(invalid("service profile changed"));
    }
    // The rollback record is persisted before the initial apply intent. If
    // interrupted between those writes, the startup barrier and unchanged old
    // definition identify that preparation boundary without guessing about a
    // completed migration or a previously completed undo.
    if operation == "--recover-migration"
        && !exists(&prefix.join(INTENT))?
        && exists(&prefix.join(INSTALL_PENDING))?
        && !exists(&prefix.join(UNDONE))?
        && observed == old
    {
        store::create(&prefix.join(INTENT), b"apply\n", false)?;
    }
    let undo = if operation == "--undo-migration" {
        if exists(&prefix.join(INTENT))? && store::read(&prefix.join(INTENT), 16)? != b"undo\n" {
            return Err(invalid(
                "finish the interrupted migration before undoing it",
            ));
        }
        if !exists(&prefix.join(INTENT))? {
            store::create(&prefix.join(INTENT), b"undo\n", false)?;
        }
        true
    } else {
        match store::read(&prefix.join(INTENT), 16)?.as_slice() {
            b"apply\n" => false,
            b"undo\n" => true,
            _ => return Err(invalid("invalid migration intent")),
        }
    };
    if !undo && exists(&prefix.join(UNDONE))? {
        return Err(invalid("migration was already undone"));
    }
    if !exists(&prefix.join(INSTALL_PENDING))? {
        store::create(
            &prefix.join(INSTALL_PENDING),
            b"Service migration must complete before startup.\n",
            false,
        )?;
    }
    legacy.recheck(&record)?;
    if store::read(&registration(&record), RECORD_LIMIT)? != observed {
        return Err(invalid("registration changed during migration"));
    }
    profile.check()?;
    manager_stopped(&record, observed == old)?;
    store::replace(&registration(&record), if undo { old } else { &new })?;
    #[cfg(target_os = "linux")]
    super::checked_manager(&["daemon-reload"])?;
    profile.check()?;
    profile.finish()?;
    if undo {
        if !exists(&prefix.join(UNDONE))? {
            store::create(
                &prefix.join(UNDONE),
                b"Legacy registration restored; native startup is blocked.\n",
                false,
            )?;
        }
        // Keep the installation barrier permanently after returning to Node.
        fs::remove_file(prefix.join(INTENT))?;
        store::sync(&prefix)?;
        println!(
            "Legacy service definition restored. Start it locally and verify its Hub connection. Native files and all rollback material were retained."
        );
    } else {
        complete(&prefix)?;
        println!(
            "Native service migration completed. Start it locally and verify its Hub connection."
        );
    }
    Ok(true)
}
