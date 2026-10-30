//! Windows managed versions use private records, never executable symlinks.
use crate::storage::windows::{
    FileIdentity, check_file_acl, create_child, delete_open_object, file_identity, open_child,
    open_child_request, open_parent, open_private_directory_for_deletion,
    open_private_file_for_deletion, require_child_absent,
};
use crate::{
    error::{Error, Result},
    random, storage, update,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{ERROR_LOCK_VIOLATION, GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        LockFileEx, READ_CONTROL, SYNCHRONIZE, UnlockFileEx,
    },
    System::IO::OVERLAPPED,
};

const MARKER: &str = ".native-install.json";
const LOCK: &str = ".manager.lock";
const RUNNING: &str = ".running";
const PROFILES: &str = ".profiles";
const PROFILES_LOCK: &str = ".profiles.lock";
const MAX_PROFILES: usize = 256;
pub const PENDING: &str = ".update-pending.json";
pub(crate) const MAX_PENDING_BYTES: usize = 256 * 1024;

pub(crate) struct Object {
    pub file: File,
    _parents: Vec<File>,
}
pub(crate) fn private_object(path: &Path, directory: bool) -> Result<Object> {
    let (parents, name) = open_parent(path, false, false)?;
    let file = open_child(parents.last().unwrap(), &name, directory, true)?;
    check_file_acl(&file)?;
    Ok(Object {
        file,
        _parents: parents,
    })
}
pub(crate) fn create_directory(path: &Path) -> Result<FileIdentity> {
    let (parents, name) = open_parent(path, false, true)?;
    let file = create_child(parents.last().unwrap(), &name, true)?;
    file_identity(&file)
}
pub(crate) fn remove_same(path: &Path, identity: FileIdentity, directory: bool) -> Result<()> {
    let (parents, name) = open_parent(path, false, true)?;
    let parent = parents.last().unwrap();
    let file = if directory {
        open_private_directory_for_deletion(parent, &name)?
    } else {
        open_private_file_for_deletion(parent, &name)?
    };
    if file_identity(&file)? != identity {
        return Err(Error::message("Native state object changed"));
    }
    delete_open_object(&file)?;
    drop(file);
    require_child_absent(parent, &name, directory)
}
fn marker_valid(prefix: &Path) -> Result<()> {
    let _root = private_object(prefix, true)?;
    let bytes = storage::read_private(&prefix.join(MARKER))?;
    if bytes.len() > 4096 {
        return Err(Error::message("Invalid native installation marker"));
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if value["schema"] != 1 || value["kind"] != "native-agent" {
        return Err(Error::message("Invalid native installation marker"));
    }
    Ok(())
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pointer {
    schema: u8,
    kind: String,
    version: String,
}
pub(crate) fn pointer_path(prefix: &Path, name: &str) -> Result<PathBuf> {
    if !matches!(name, "current" | "previous") {
        return Err(Error::message("Invalid native version pointer"));
    }
    Ok(prefix.join(format!("{name}.json")))
}
pub fn pointer(prefix: &Path, name: &str) -> Result<String> {
    let bytes = storage::read_private(&pointer_path(prefix, name)?)?;
    if bytes.len() > 4096 {
        return Err(Error::message("Invalid native version pointer"));
    }
    let value: Pointer = serde_json::from_slice(&bytes)?;
    if value.schema != 1 || value.kind != "native-version" || !update::valid_version(&value.version)
    {
        return Err(Error::message("Invalid native version pointer"));
    }
    Ok(value.version)
}
pub(crate) fn set_pointer(prefix: &Path, name: &str, version: Option<&str>) -> Result<()> {
    let path = pointer_path(prefix, name)?;
    if let Some(version) = version {
        if !update::valid_version(version) {
            return Err(Error::message("Invalid native version"));
        }
        let data = serde_json::to_vec(&Pointer {
            schema: 1,
            kind: "native-version".into(),
            version: version.into(),
        })?;
        storage::write_private_replace(&path, &data)
    } else {
        match private_object(&path, false) {
            Ok(object) => {
                pointer(prefix, name)?;
                let identity = file_identity(&object.file)?;
                drop(object);
                remove_same(&path, identity, false)
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
pub(crate) fn version_binary(prefix: &Path, version: &str) -> Result<PathBuf> {
    if !update::valid_version(version) {
        return Err(Error::message("Invalid native version"));
    }
    let _versions = private_object(&prefix.join("versions"), true)?;
    let directory = prefix.join("versions").join(version);
    let _version = private_object(&directory, true)?;
    let path = directory.join("rdh.exe");
    let _binary = private_object(&path, false)?;
    Ok(path)
}
fn verify_version(prefix: &Path, version: &str, executable: &Path) -> Result<()> {
    let path = version_binary(prefix, version)?;
    let expected = private_object(&path, false)?;
    let actual = File::open(executable)?;
    if file_identity(&expected.file)? != file_identity(&actual)? {
        return Err(Error::message(
            "This executable does not match the selected native version",
        ));
    }
    Ok(())
}

pub(crate) fn command_target(prefix: &Path) -> Result<(PathBuf, Object)> {
    let _lock = lock_file(prefix, LOCK, false)?;
    let version = pointer(prefix, "current")?;
    let binary = version_binary(prefix, &version)?;
    let pinned = crate::native_install::pin_stored(prefix, &version)?;
    // Pending-state policy belongs to the selected command; recovery must
    // remain callable through the same stable entry point.
    Ok((binary, pinned))
}
pub(crate) fn pending(prefix: &Path) -> Result<Value> {
    let bytes = storage::read_private(&prefix.join(PENDING))?;
    if bytes.len() > MAX_PENDING_BYTES {
        return Err(Error::message("Invalid native update record"));
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let valid = |key: &str| value[key].as_str().is_some_and(update::valid_version);
    if value["schema"] != 1
        || !value
            .as_object()
            .is_some_and(|value| value.contains_key("old_previous"))
        || !valid("old_current")
        || !valid("new_current")
        || (!value["old_previous"].is_null() && !valid("old_previous"))
    {
        return Err(Error::message("Invalid native update record"));
    }
    Ok(value)
}
fn absent(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}
fn ensure_no_service_pending(prefix: &Path) -> Result<()> {
    if !absent(&prefix.join(".service-update.json"))? {
        return Err(Error::message(
            "A service update is pending; inspect and recover it before managing the installation",
        ));
    }
    Ok(())
}
fn ensure_no_pending(prefix: &Path) -> Result<()> {
    ensure_no_service_pending(prefix)?;
    if !absent(&prefix.join(PENDING))? {
        return Err(Error::message(
            "A native update was interrupted; recover it before starting the Agent",
        ));
    }
    Ok(())
}

struct FileLock {
    object: Object,
}
impl Drop for FileLock {
    fn drop(&mut self) {
        // This handle is non-inheritable; explicitly unlock before closing it.
        let mut overlap = OVERLAPPED::default();
        unsafe {
            UnlockFileEx(self.object.file.as_raw_handle(), 0, 1, 0, &mut overlap);
        }
    }
}
fn lock_file(prefix: &Path, name: &str, exclusive: bool) -> Result<FileLock> {
    marker_valid(prefix)?;
    let path = prefix.join(name);
    if absent(&path)? {
        match storage::write_private_new(&path, b"") {
            Ok(()) => {}
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let (parents, name) = open_parent(&path, false, true)?;
    let file = open_child_request(
        parents.last().unwrap(),
        &name,
        false,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )?;
    check_file_acl(&file)?;
    if file.metadata()?.len() != 0 {
        return Err(Error::message("Invalid native installation lock"));
    }
    let mut overlap = OVERLAPPED::default();
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if exclusive {
            LOCKFILE_EXCLUSIVE_LOCK
        } else {
            0
        };
    // Byte-range locks may extend beyond EOF. Shared Agent owners exclude an
    // updater's exclusive lock; no PID lookup substitutes for this kernel lock.
    if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlap) } == 0 {
        let error = io::Error::last_os_error();
        return Err(
            if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                Error::message(
                    "A native Agent or installation manager is active; finish its tasks and stop it before updating",
                )
            } else {
                error.into()
            },
        );
    }
    Ok(FileLock {
        object: Object {
            file,
            _parents: parents,
        },
    })
}
fn private_subdirectory(prefix: &Path, name: &str) -> Result<PathBuf> {
    let path = prefix.join(name);
    match create_directory(&path) {
        Ok(_) => {}
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            private_object(&path, true)?;
        }
        Err(error) => return Err(error),
    }
    Ok(path)
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
    if value["schema"] != 1 || path.file_name().and_then(|value| value.to_str()) != Some(&expected)
    {
        return Err(Error::message("Invalid native profile record"));
    }
    Ok(PathBuf::from(config))
}
fn registered_profiles(prefix: &Path) -> Result<Vec<PathBuf>> {
    let path = private_subdirectory(prefix, PROFILES)?;
    let _directory = private_object(&path, true)?;
    let mut result = Vec::new();
    for entry in fs::read_dir(&path)? {
        if result.len() >= MAX_PROFILES {
            return Err(Error::message("Native installation has too many profiles"));
        }
        result.push(profile_from_file(&entry?.path())?);
    }
    result.sort();
    Ok(result)
}
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
    if !absent(&path)? {
        return if profile_from_file(&path)? == config {
            Ok(())
        } else {
            Err(Error::message("Native profile record changed"))
        };
    }
    if registered_profiles(prefix)?.len() >= MAX_PROFILES {
        return Err(Error::message("Native installation has too many profiles"));
    }
    let bytes = serde_json::to_vec(&json!({"schema":1,"config":text}))?;
    if bytes.len() > 8192 {
        return Err(Error::message("Native profile record is too large"));
    }
    storage::write_private_new(&path, &bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningRecord {
    pub pid: u32,
    pub config: PathBuf,
    pub nonce: String,
}
fn read_marker(path: &Path) -> Result<(RunningRecord, FileIdentity)> {
    let object = private_object(path, false)?;
    let bytes = storage::read_private(path)?;
    if bytes.len() > 8192 {
        return Err(Error::message("Invalid native running marker"));
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let pid = value["pid"]
        .as_u64()
        .filter(|pid| *pid > 0 && *pid <= u32::MAX as u64)
        .ok_or_else(|| Error::message("Invalid native running marker"))? as u32;
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
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        })
        .ok_or_else(|| Error::message("Invalid native running marker"))?;
    if value["schema"] != 1
        || path.file_name().and_then(|value| value.to_str()) != Some(&format!("{pid}-{nonce}.json"))
    {
        return Err(Error::message("Invalid native running marker"));
    }
    Ok((
        RunningRecord {
            pid,
            // Earlier markers could store a Task's ordinary Win32 spelling,
            // while profile registration has always stored the canonical path.
            // Resolve the same file before comparing; never infer equivalence
            // by removing a prefix or changing case in a string.
            config: Path::new(config).canonicalize()?,
            nonce: nonce.into(),
        },
        file_identity(&object.file)?,
    ))
}
fn markers(prefix: &Path) -> Result<Vec<(PathBuf, RunningRecord, FileIdentity)>> {
    let path = private_subdirectory(prefix, RUNNING)?;
    let _directory = private_object(&path, true)?;
    let mut result = Vec::new();
    for entry in fs::read_dir(&path)? {
        if result.len() >= MAX_PROFILES {
            return Err(Error::message(
                "Native installation has too many running markers",
            ));
        }
        let path = entry?.path();
        let (record, identity) = read_marker(&path)?;
        result.push((path, record, identity));
    }
    Ok(result)
}
pub struct RunningInstall {
    prefix: PathBuf,
    _lock: FileLock,
    marker: Option<(PathBuf, FileIdentity)>,
}
impl RunningInstall {
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }
    pub fn acquire_current() -> Result<Option<Self>> {
        let Some((prefix, executable)) = current_installation()? else {
            return Ok(None);
        };
        let lock = lock_file(&prefix, LOCK, false)?;
        ensure_no_pending(&prefix)?;
        verify_version(&prefix, &pointer(&prefix, "current")?, &executable)?;
        Ok(Some(Self {
            prefix,
            _lock: lock,
            marker: None,
        }))
    }
    pub fn acquire_launch_current() -> Result<Option<Self>> {
        Self::acquire_current()
    }
    pub fn check_launch_profile(&self, _: &Path, _: bool) -> Result<()> {
        Ok(())
    }
    pub fn finish_launch(&mut self) {}
    pub fn register_profile(&self, config: &Path) -> Result<()> {
        record_profile(&self.prefix, config)
    }
    pub fn mark_running(&mut self, config: &Path, recover: bool) -> Result<()> {
        let config = config.canonicalize()?;
        let existing = markers(&self.prefix)?;
        let selected: Vec<_> = existing
            .iter()
            .filter(|(_, record, _)| record.config == config)
            .collect();
        if !selected.is_empty() {
            if !recover {
                return Err(Error::message(
                    "A native running marker remains for this profile; inspect and recover the previous Agent before starting",
                ));
            }
            let owners: Vec<_> = selected.iter().map(|(_, record, _)| record.pid).collect();
            crate::profile_lock::require_dead_owners(&owners)?;
            for (path, _, identity) in selected {
                remove_same(path, *identity, false)?;
            }
        }
        let text = config
            .to_str()
            .ok_or_else(|| Error::message("Config path must be Unicode"))?;
        let nonce = random::hex_token::<16>()?;
        let pid = std::process::id();
        let path = self
            .prefix
            .join(RUNNING)
            .join(format!("{pid}-{nonce}.json"));
        storage::write_private_new(
            &path,
            &serde_json::to_vec(&json!({"schema":1,"pid":pid,"config":text,"nonce":nonce}))?,
        )?;
        let object = private_object(&path, false)?;
        self.marker = Some((path, file_identity(&object.file)?));
        Ok(())
    }
    pub fn mark_clean(&mut self) -> Result<()> {
        if let Some((path, identity)) = self.marker.take() {
            remove_same(&path, identity, false)?;
        }
        Ok(())
    }
}
pub fn current_installation() -> Result<Option<(PathBuf, PathBuf)>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let Some(version) = executable.parent() else {
        return Ok(None);
    };
    let Some(versions) = version.parent().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("versions"))
    }) else {
        return Ok(None);
    };
    let prefix = versions
        .parent()
        .ok_or_else(|| Error::message("Invalid native installation prefix"))?;
    if absent(&prefix.join(MARKER))? {
        return Ok(None);
    }
    Ok(Some((prefix.to_path_buf(), executable)))
}
pub struct ManagerLock {
    pub prefix: PathBuf,
    _lock: FileLock,
}
impl ManagerLock {
    pub fn acquire(prefix: &Path, executable: &Path, allow_pending: bool) -> Result<Self> {
        Self::acquire_with_stale_recovery(prefix, executable, allow_pending, false)
    }
    pub fn acquire_with_stale_recovery(
        prefix: &Path,
        executable: &Path,
        allow_pending: bool,
        recover: bool,
    ) -> Result<Self> {
        let lock = lock_file(prefix, LOCK, true)?;
        ensure_no_service_pending(prefix)?;
        if allow_pending {
            let pending = pending(prefix)?;
            if verify_version(prefix, pending["old_current"].as_str().unwrap(), executable).is_err()
                && verify_version(prefix, pending["new_current"].as_str().unwrap(), executable)
                    .is_err()
            {
                return Err(Error::message(
                    "Recovery executable is not part of the pending update",
                ));
            }
        } else {
            ensure_no_pending(prefix)?;
            verify_version(prefix, &pointer(prefix, "current")?, executable)?;
        }
        let existing = markers(prefix)?;
        if recover {
            let owners: Vec<_> = existing.iter().map(|(_, record, _)| record.pid).collect();
            crate::profile_lock::require_dead_owners(&owners)?;
            // An interrupted manager can leave profile/startup locks even
            // without any Agent running marker. Recover all registered paths
            // together so a live owner prevents partial stale-lock removal.
            let mut paths = registered_profiles(prefix)?;
            if existing
                .iter()
                .any(|(_, record, _)| !paths.contains(&record.config))
            {
                return Err(Error::message(
                    "Running marker has no registered native profile",
                ));
            }
            paths.extend(
                paths
                    .clone()
                    .iter()
                    .map(|path| crate::startup::sidecar_path(path)),
            );
            crate::profile_lock::ProfileLocks::recover(&paths)?;
            for (path, _, identity) in existing {
                remove_same(&path, identity, false)?;
            }
        } else if !existing.is_empty() {
            return Err(Error::message(
                "Native Agent running or unclean-shutdown markers remain; inspect and recover their profile locks before updating",
            ));
        }
        Ok(Self {
            prefix: prefix.into(),
            _lock: lock,
        })
    }
    pub fn profiles(&self) -> Result<Vec<PathBuf>> {
        registered_profiles(&self.prefix)
    }
    pub fn forget_profile(&self, config: &Path) -> Result<()> {
        let text = config
            .to_str()
            .filter(|_| config.is_absolute())
            .ok_or_else(|| Error::message("Profile path must be absolute Unicode"))?;
        let path = private_subdirectory(&self.prefix, PROFILES)?.join(format!(
            "{}.json",
            crate::config::sha256_hex(text.as_bytes())
        ));
        let object = private_object(&path, false)?;
        if profile_from_file(&path)? != config {
            return Err(Error::message("Native profile record changed"));
        }
        let identity = file_identity(&object.file)?;
        drop(object);
        let _profile_lock = if config.parent().is_some_and(Path::is_dir) {
            Some(crate::profile_lock::ProfileLocks::acquire(&[
                config.to_path_buf()
            ])?)
        } else {
            None
        };
        remove_same(&path, identity, false)
    }
}
