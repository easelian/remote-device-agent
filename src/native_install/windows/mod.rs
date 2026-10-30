//! Private Windows version directories and recoverable offline selection.
//! Executables are never overwritten and installation records are not launchers.
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};

use crate::startup::{UpdateTask, prepare_update_task, switch_update_task, verify_update_task};
use crate::storage::windows::{
    FileIdentity, create_child, file_identity, open_install_parent, open_parent,
    open_private_directory_for_deletion, rename_file, require_child_absent,
};
use crate::{
    config::sha256_hex,
    error::{Error, Result},
    install_lock::{
        ManagerLock, PENDING, pending, pointer, pointer_path, private_object, record_profile,
        remove_same, set_pointer, version_binary,
    },
    profile_lock::ProfileLocks,
    random, storage, update,
};

const VERSION_MANIFEST: &str = ".native-version.json";

pub(crate) fn check_executable(bytes: &[u8]) -> Result<()> {
    let invalid = || Error::message("Native update must be a matching 64-bit Windows executable");
    if bytes.len() < 64 || &bytes[..2] != b"MZ" {
        return Err(invalid());
    }
    let offset = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
    let end = offset
        .checked_add(26)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(invalid)?;
    let header = &bytes[offset..end];
    let machine = match std::env::consts::ARCH {
        "x86_64" => 0x8664,
        "aarch64" => 0xaa64,
        _ => return Err(invalid()),
    };
    let characteristics = u16::from_le_bytes(header[22..24].try_into().unwrap());
    if &header[..4] != b"PE\0\0"
        || u16::from_le_bytes(header[4..6].try_into().unwrap()) != machine
        || u16::from_le_bytes(header[20..22].try_into().unwrap()) < 2
        || u16::from_le_bytes(header[24..26].try_into().unwrap()) != 0x20b
        || characteristics & 0x0002 == 0
        || characteristics & 0x2000 != 0
    {
        return Err(invalid());
    }
    Ok(())
}

fn check_version(path: &Path, version: &str) -> Result<()> {
    // Keep the binary and every ancestor non-replaceable for the whole check.
    let _binary = private_object(path, false)?;
    crate::processes::check_native_program(
        path,
        &["--version"],
        version,
        false,
        Duration::from_secs(15),
    )?;
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

// Track only objects created by this stage. Cleanup never recursively deletes
// arbitrary names; a replaced object or extra child leaves the stage for review.
struct Stage {
    path: PathBuf,
    parents: Vec<File>,
    name: Vec<u16>,
    identity: FileIdentity,
    created: Vec<(PathBuf, FileIdentity, bool)>,
    published: bool,
}
impl Stage {
    fn new(parent: &Path, label: &str) -> Result<Self> {
        let path = parent.join(format!(".{label}.{}.tmp", random::hex_token::<16>()?));
        let (parents, name) = open_install_parent(&path)?;
        let root = create_child(parents.last().unwrap(), &name, true)?;
        let identity = file_identity(&root)?;
        drop(root);
        Ok(Self {
            path,
            parents,
            name,
            identity,
            created: Vec::new(),
            published: false,
        })
    }
    fn directory(&mut self, relative: &Path) -> Result<()> {
        let path = self.path.join(relative);
        let (parents, name) = open_parent(&path, false, true)?;
        let file = create_child(parents.last().unwrap(), &name, true)?;
        self.created.push((path, file_identity(&file)?, true));
        Ok(())
    }
    fn file(&mut self, relative: &Path, bytes: &[u8]) -> Result<()> {
        let path = self.path.join(relative);
        let (parents, name) = open_parent(&path, false, true)?;
        let mut file = create_child(parents.last().unwrap(), &name, false)?;
        self.created.push((path, file_identity(&file)?, false));
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    fn metadata(&mut self, directory: &Path, version: &str, bytes: &[u8]) -> Result<()> {
        self.file(
            &directory.join(VERSION_MANIFEST),
            &serde_json::to_vec(&json!({
                "schema":1,"kind":"native-binary","version":version,
                "binary":{"sha256":sha256_hex(bytes),"bytes":bytes.len()}
            }))?,
        )
    }
    fn profile(&mut self, config: &Path) -> Result<()> {
        record_profile(&self.path, config)?;
        let text = config.canonicalize()?;
        let text = text
            .to_str()
            .ok_or_else(|| Error::message("Config path must be Unicode"))?;
        for (relative, directory) in [
            (PathBuf::from(".profiles.lock"), false),
            (PathBuf::from(".profiles"), true),
            (
                PathBuf::from(".profiles").join(format!("{}.json", sha256_hex(text.as_bytes()))),
                false,
            ),
        ] {
            let path = self.path.join(relative);
            let object = private_object(&path, directory)?;
            self.created
                .push((path, file_identity(&object.file)?, directory));
        }
        Ok(())
    }
    fn publish(mut self, destination: &Path) -> Result<()> {
        let (parents, name) = open_install_parent(destination)?;
        let source_parent = self.parents.last().unwrap();
        if file_identity(source_parent)? != file_identity(parents.last().unwrap())? {
            return Err(Error::message("Native stage publication parent changed"));
        }
        let directory = open_private_directory_for_deletion(source_parent, &self.name)?;
        if file_identity(&directory)? != self.identity {
            return Err(Error::message("Native stage changed before publication"));
        }
        // A failed rename can have an uncertain outcome. Never delete through
        // the publishing handle or assume the destination remained absent.
        self.published = true;
        rename_file(&directory, parents.last().unwrap(), &name, false).map_err(|error| {
            Error::message(format!("Native publication could not be confirmed: {error}; inspect {} and {} before retrying", self.path.display(), destination.display()))
        })?;
        drop(directory);
        let published = private_object(destination, true)?;
        if file_identity(&published.file)? != self.identity {
            return Err(Error::message(
                "Native publication identity could not be confirmed",
            ));
        }
        Ok(())
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        // Pin the original directory while removing only its recorded children.
        let Ok(root) = private_object(&self.path, true) else {
            return;
        };
        if file_identity(&root.file).ok() != Some(self.identity) {
            return;
        }
        for (path, identity, directory) in self.created.iter().rev() {
            if remove_same(path, *identity, *directory).is_err() {
                return;
            }
        }
        drop(root);
        let parent = self.parents.last().unwrap();
        if let Ok(root) = open_private_directory_for_deletion(parent, &self.name)
            && file_identity(&root).ok() == Some(self.identity)
        {
            let _ = storage::windows::delete_open_object(&root);
            drop(root);
            let _ = require_child_absent(parent, &self.name, true);
        }
    }
}

fn checked_bytes(path: &Path) -> Result<Vec<u8>> {
    let object = private_object(path, false)?;
    if object.file.metadata()?.len() > update::MAX_BINARY_BYTES {
        return Err(Error::message("Native binary size is invalid"));
    }
    let mut bytes = Vec::new();
    (&object.file)
        .take(update::MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > update::MAX_BINARY_BYTES {
        return Err(Error::message("Native binary size is invalid"));
    }
    Ok(bytes)
}

pub(crate) fn pin_stored(prefix: &Path, version: &str) -> Result<crate::install_lock::Object> {
    let path = version_binary(prefix, version)?;
    let binary = private_object(&path, false)?;
    let manifest: Value = serde_json::from_slice(&storage::read_private(
        &path.parent().unwrap().join(VERSION_MANIFEST),
    )?)?;
    let bytes = checked_bytes(&path)?;
    if manifest["schema"] != 1
        || manifest["kind"] != "native-binary"
        || manifest["version"] != version
        || manifest["binary"]["bytes"].as_u64() != Some(bytes.len() as u64)
        || manifest["binary"]["sha256"].as_str() != Some(&sha256_hex(&bytes))
    {
        return Err(Error::message(
            "Native version size or checksum does not match its metadata",
        ));
    }
    check_executable(&bytes)?;
    Ok(binary)
}

pub fn verify_stored(prefix: &Path, version: &str) -> Result<()> {
    let _binary = pin_stored(prefix, version)?;
    let path = version_binary(prefix, version)?;
    check_version(&path, version)
}

pub fn stage(manager: &ManagerLock, version: &str, bytes: &[u8], digest: &str) -> Result<()> {
    if !update::valid_version(version)
        || bytes.is_empty()
        || bytes.len() as u64 > update::MAX_BINARY_BYTES
        || sha256_hex(bytes) != digest
    {
        return Err(Error::message("Native update size or checksum is invalid"));
    }
    check_executable(bytes)?;
    let versions = manager.prefix.join("versions");
    let _versions = private_object(&versions, true)?;
    let destination = versions.join(version);
    if exists(&destination)? {
        verify_stored(&manager.prefix, version)?;
        if checked_bytes(&destination.join("rdh.exe"))? == bytes {
            return Ok(());
        }
        return Err(Error::message(
            "A different native version already exists; it was not overwritten",
        ));
    }
    let mut temporary = Stage::new(&versions, "native-version")?;
    temporary.file(Path::new("rdh.exe"), bytes)?;
    temporary.metadata(Path::new(""), version, bytes)?;
    check_version(&temporary.path.join("rdh.exe"), version)?;
    temporary.publish(&destination)
}

fn install_inner(prefix: &Path, profile: Option<&Path>) -> Result<PathBuf> {
    if !prefix.is_absolute() {
        return Err(Error::message(
            "Native installation prefix must be absolute",
        ));
    }
    // Existing ancestors must be resolvable without reparse points. The
    // immediate parent may grant public reads, but no untrusted modification.
    let (_parents, _) = open_install_parent(prefix)?;
    if exists(prefix)? {
        return Err(Error::message(
            "Installation prefix already exists; existing installations are not overwritten or migrated",
        ));
    }
    let parent = prefix
        .parent()
        .ok_or_else(|| Error::message("Invalid native installation prefix"))?;
    let executable = std::env::current_exe()?;
    let mut bytes = Vec::new();
    File::open(executable)?
        .take(update::MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > update::MAX_BINARY_BYTES {
        return Err(Error::message("Native binary size is invalid"));
    }
    check_executable(&bytes)?;
    let mut temporary = Stage::new(parent, "rdh-install")?;
    temporary.directory(Path::new("versions"))?;
    let version = Path::new("versions").join(env!("CARGO_PKG_VERSION"));
    temporary.directory(&version)?;
    temporary.file(&version.join("rdh.exe"), &bytes)?;
    temporary.metadata(&version, env!("CARGO_PKG_VERSION"), &bytes)?;
    check_version(
        &temporary.path.join(&version).join("rdh.exe"),
        env!("CARGO_PKG_VERSION"),
    )?;
    temporary.file(
        Path::new(".native-install.json"),
        br#"{"schema":1,"kind":"native-agent"}"#,
    )?;
    temporary.file(
        Path::new("install-id"),
        format!("{}\n", random::hex_token::<16>()?).as_bytes(),
    )?;
    temporary.file(
        Path::new("current.json"),
        &serde_json::to_vec(
            &json!({"schema":1,"kind":"native-version","version":env!("CARGO_PKG_VERSION")}),
        )?,
    )?;
    for name in crate::native_launcher::COMMAND_NAMES {
        temporary.file(Path::new(&format!("{name}.exe")), &bytes)?;
    }
    if let Some(config) = profile {
        temporary.profile(config)?;
    }
    temporary.publish(prefix)?;
    Ok(prefix.to_path_buf())
}

pub fn install(prefix: &Path) -> Result<PathBuf> {
    install_inner(prefix, None)
}
pub(crate) fn install_for_locked_profile(prefix: &Path, profile: &Path) -> Result<PathBuf> {
    install_inner(prefix, Some(profile))
}

pub fn validate_profiles(manager: &ManagerLock, version: &str) -> Result<ProfileLocks> {
    let profiles = manager.profiles()?;
    for config in &profiles {
        storage::read_private(config)?;
    }
    let locks = ProfileLocks::acquire(&profiles)?;
    let path = version_binary(&manager.prefix, version)?;
    let _binary = private_object(&path, false)?;
    for config in &profiles {
        let name = config
            .to_str()
            .ok_or_else(|| Error::message("Invalid native profile path"))?;
        crate::processes::check_native_program(
            &path,
            &["validate-config", "--config", name],
            "configuration valid",
            true,
            Duration::from_secs(30),
        )
        .map_err(|_| {
            Error::message(format!(
                "Native version cannot validate registered profile: {}",
                config.display()
            ))
        })?;
    }
    Ok(locks)
}

pub fn previous(prefix: &Path) -> Result<Option<String>> {
    if exists(&pointer_path(prefix, "previous")?)? {
        Ok(Some(pointer(prefix, "previous")?))
    } else {
        Ok(None)
    }
}
fn clear_pending(prefix: &Path) -> Result<()> {
    let path = prefix.join(PENDING);
    let object = private_object(&path, false)?;
    pending(prefix)?;
    let identity = file_identity(&object.file)?;
    drop(object);
    remove_same(&path, identity, false)
}
fn startup_locks(manager: &ManagerLock) -> Result<ProfileLocks> {
    let paths = manager
        .profiles()?
        .iter()
        .map(|path| crate::startup::sidecar_path(path))
        .collect::<Vec<_>>();
    ProfileLocks::acquire(&paths)
}
fn prepare_targets(manager: &ManagerLock, old: &str, new: &str) -> Result<Vec<UpdateTask>> {
    let old = version_binary(&manager.prefix, old)?;
    let new = version_binary(&manager.prefix, new)?;
    let mut targets = Vec::new();
    for config in manager.profiles()? {
        if let (Some(binding), _) = crate::startup::inspect_sidecar(&config)? {
            let target = prepare_update_task(&config, &binding, &old, &new)?;
            if targets
                .iter()
                .any(|previous: &UpdateTask| previous.name == target.name)
            {
                return Err(Error::message(
                    "Multiple profiles refer to the same startup task",
                ));
            }
            targets.push(target);
        }
    }
    Ok(targets)
}
fn recorded_targets(manager: &ManagerLock, value: &Value) -> Result<Vec<UpdateTask>> {
    let targets: Vec<UpdateTask> =
        serde_json::from_value(value.get("tasks").cloned().unwrap_or_else(|| json!([])))?;
    let profiles = manager.profiles()?;
    if targets.len() > profiles.len() {
        return Err(Error::message("Invalid native startup update record"));
    }
    let old = version_binary(&manager.prefix, value["old_current"].as_str().unwrap())?;
    let new = version_binary(&manager.prefix, value["new_current"].as_str().unwrap())?;
    let mut configs = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for target in &targets {
        if !profiles.contains(&target.config)
            || !configs.insert(target.config.clone())
            || !names.insert(&target.name)
        {
            return Err(Error::message(
                "Invalid or duplicated native startup update record",
            ));
        }
        verify_update_task(target, &old, &new)?;
    }
    for config in profiles {
        if crate::startup::inspect_sidecar(&config)?.0.is_some() && !configs.contains(&config) {
            return Err(Error::message(
                "Pending update does not account for a registered startup binding",
            ));
        }
    }
    Ok(targets)
}
fn restore(manager: &ManagerLock) -> Result<()> {
    let prefix = &manager.prefix;
    let value = pending(prefix)?;
    let old = value["old_current"].as_str().unwrap();
    let new = value["new_current"].as_str().unwrap();
    let prior = value["old_previous"].as_str();
    version_binary(prefix, old)?;
    if let Some(prior) = prior {
        version_binary(prefix, prior)?;
    }
    let current = pointer(prefix, "current")?;
    if current != old && current != new {
        return Err(Error::message(
            "Current version is unrelated to the interrupted update; inspect before recovery",
        ));
    }
    let actual_previous = previous(prefix)?;
    if actual_previous.as_deref() != prior && actual_previous.as_deref() != Some(old) {
        return Err(Error::message(
            "Previous version changed during the interrupted update; inspect before recovery",
        ));
    }
    // Validate every registration before any restoration. A changed external
    // task leaves the journal and all startup selectors for explicit review.
    let targets = recorded_targets(manager, &value)?;
    for target in &targets {
        switch_update_task(target, true)?;
    }
    set_pointer(prefix, "previous", prior)?;
    set_pointer(prefix, "current", Some(old))?;
    clear_pending(prefix)
}
pub fn recover(manager: &ManagerLock) -> Result<()> {
    let _profiles = ProfileLocks::acquire(&manager.profiles()?)?;
    let _startup = startup_locks(manager)?;
    restore(manager)
}
pub fn switch(manager: &ManagerLock, target: &str) -> Result<()> {
    let prefix = &manager.prefix;
    let old = pointer(prefix, "current")?;
    if old == target {
        return Err(Error::message("The target version is already current"));
    }
    version_binary(prefix, &old)?;
    version_binary(prefix, target)?;
    let prior = previous(prefix)?;
    if let Some(prior) = &prior {
        version_binary(prefix, prior)?;
    }
    let _startup = startup_locks(manager)?;
    let targets = prepare_targets(manager, &old, target)?;
    let record = serde_json::to_vec(
        &json!({"schema":1,"old_current":old,"new_current":target,"old_previous":prior,"tasks":targets}),
    )?;
    if record.len() > crate::install_lock::MAX_PENDING_BYTES {
        return Err(Error::message(
            "Native startup update record exceeds its size limit",
        ));
    }
    storage::write_private_new(&prefix.join(PENDING), &record)?;
    let outcome = (|| -> Result<()> {
        for target in &targets {
            switch_update_task(target, false)?;
        }
        set_pointer(prefix, "previous", Some(&old))?;
        set_pointer(prefix, "current", Some(target))
    })();
    if let Err(error) = outcome {
        return match restore(manager) {
            Ok(()) => Err(Error::message(format!(
                "Native switch failed and was restored: {error}"
            ))),
            Err(_) => Err(Error::message(
                "Native switch was interrupted; keep the installation stopped and run recovery",
            )),
        };
    }
    clear_pending(prefix).map_err(|_| Error::message("Native switch completed but its recovery record could not be cleared; inspect before starting"))
}
pub fn pending_record(prefix: &Path) -> Result<Value> {
    pending(prefix)
}
