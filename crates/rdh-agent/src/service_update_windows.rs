//! Offline updates for the administrator-owned LocalService installation.
//! A shared launch lock excludes the controller. A durable pending record
//! refuses launch after interruption until an explicit, checked recovery.
use std::{
    fs::File,
    io::{self, Read, Write},
    mem::size_of,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_LOCK_VIOLATION, GENERIC_READ},
    Storage::FileSystem::{
        DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, READ_CONTROL, SYNCHRONIZE, UnlockFileEx,
    },
    System::{
        IO::OVERLAPPED,
        Services::{
            ChangeServiceConfigW, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW,
            QueryServiceConfigW, QueryServiceStatusEx, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
            SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_DEMAND_START, SERVICE_DISABLED,
            SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_STATUS_PROCESS,
            SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
        },
    },
};

use super::{Creation, Privilege, Service, wide};
use crate::{
    cli::LaunchArgs,
    config::{same_hub, sha256_hex},
    error::{Error, Result},
    native_install, random,
    startup::valid_name,
    startup_native::windows::{administrator, startup_path},
    storage::windows::{
        account_matches_sid, check_service_object, create_service_child, current_sid_text,
        delete_open_object, file_identity, named_identity, open_child, open_child_request,
        open_service_install_parent, rename_file, require_child_absent,
    },
    update::{MAX_BINARY_BYTES, valid_version},
};

pub(super) const MANIFEST: &str = ".service-install.json";
pub(super) const LOCK: &str = ".service-manager.lock";
const PENDING: &str = ".service-update-pending.json";
const RECORD_LIMIT: usize = 128 * 1024;

fn invalid() -> Error {
    Error::message("Invalid native service installation or recovery record")
}
fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Version {
    pub version: String,
    sha256: String,
    bytes: u64,
}
impl Version {
    fn validate(&self) -> Result<()> {
        if !valid_version(&self.version)
            || !hex(&self.sha256, 64)
            || self.bytes == 0
            || self.bytes > MAX_BINARY_BYTES
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    schema: u32,
    kind: String,
    service: String,
    account: String,
    config: PathBuf,
    server: String,
    pub install_id: String,
    pub current: Version,
    previous: Option<Version>,
}
impl Record {
    pub(super) fn new(
        service: String,
        config: PathBuf,
        server: String,
        version: &str,
        bytes: &[u8],
    ) -> Result<Self> {
        let result = Self {
            schema: 2,
            kind: "native-agent-service".into(),
            service,
            account: "S-1-5-19".into(),
            config,
            server,
            install_id: random::hex_token::<16>()?,
            current: Version {
                version: version.into(),
                sha256: sha256_hex(bytes),
                bytes: bytes.len() as u64,
            },
            previous: None,
        };
        result.validate()?;
        if serde_json::to_vec(&result)?.len() > RECORD_LIMIT {
            return Err(invalid());
        }
        Ok(result)
    }
    fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.server).map_err(|_| invalid())?;
        if self.schema != 2
            || self.kind != "native-agent-service"
            || !valid_name(&self.service)
            || self.account != "S-1-5-19"
            || !self.config.is_absolute()
            || self.config.parent().is_none()
            || !hex(&self.install_id, 32)
            || !["http", "https"].contains(&url.scheme())
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || self.server.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        startup_path(&self.config)?;
        self.current.validate()?;
        if let Some(previous) = &self.previous {
            previous.validate()?;
            if previous.version == self.current.version {
                return Err(invalid());
            }
        }
        Ok(())
    }
    fn binary(&self, prefix: &Path) -> PathBuf {
        binary(prefix, &self.current)
    }
    pub(super) fn command(&self, prefix: &Path) -> Result<String> {
        self.validate()?;
        let executable = startup_path(&self.binary(prefix))?;
        let config = startup_path(&self.config)?;
        let state = startup_path(self.config.parent().ok_or_else(invalid)?)?;
        Ok([
            executable.as_str(),
            "service-run",
            "--service-name",
            &self.service,
            "--config",
            &config,
            "--server",
            &self.server,
            "--state-directory",
            &state,
            "--recover-lock",
        ]
        .into_iter()
        .map(crate::processes::windows_argument)
        .collect::<Vec<_>>()
        .join(" "))
    }
    fn with_version(&self, version: Version) -> Self {
        let mut next = self.clone();
        next.previous = Some(self.current.clone());
        next.current = version;
        next
    }
}
fn binary(prefix: &Path, version: &Version) -> PathBuf {
    prefix
        .join("versions")
        .join(&version.version)
        .join("rdh.exe")
}

// Program objects and every ancestor stay pinned without delete sharing.
// The record is read under a kernel lock; data never comes from an LS-writable
// program manifest or a user-owned replacement of a checked ancestor.
struct Object {
    file: File,
    _parents: Vec<File>,
}
fn object(prefix: &Path, path: &Path) -> Result<Object> {
    let (parents, name) = open_service_install_parent(path)?;
    let depth = path
        .strip_prefix(prefix)
        .map_err(|_| invalid())?
        .components()
        .count();
    if depth == 0 || parents.len() < depth {
        return Err(invalid());
    }
    for parent in parents.iter().rev().take(depth) {
        check_service_object(parent, true)?;
    }
    let file = open_child(parents.last().unwrap(), &name, false, true)?;
    check_service_object(&file, true)?;
    Ok(Object {
        file,
        _parents: parents,
    })
}
fn read_record<T: serde::de::DeserializeOwned>(prefix: &Path, name: &str) -> Result<T> {
    let mut object = object(prefix, &prefix.join(name))?;
    if object.file.metadata()?.len() > RECORD_LIMIT as u64 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    (&mut object.file)
        .take(RECORD_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > RECORD_LIMIT {
        return Err(invalid());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn verify_binary(prefix: &Path, version: &Version) -> Result<Object> {
    version.validate()?;
    let mut object = object(prefix, &binary(prefix, version))?;
    if object.file.metadata()?.len() != version.bytes {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    (&mut object.file)
        .take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != version.bytes || sha256_hex(&bytes) != version.sha256 {
        return Err(Error::message(
            "Stored service executable does not match its installation record",
        ));
    }
    native_install::check_executable(&bytes)?;
    Ok(object)
}

fn write_record<T: Serialize>(prefix: &Path, name: &str, value: &T, replace: bool) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > RECORD_LIMIT {
        return Err(invalid());
    }
    let (parents, destination) = open_service_install_parent(&prefix.join(name))?;
    let parent = parents.last().unwrap();
    check_service_object(parent, true)?;
    let temporary: Vec<u16> = format!(".{name}.{}.tmp", random::hex_token::<16>()?)
        .encode_utf16()
        .collect();
    let mut privilege = Privilege::owner_assignment()?;
    let mut file = create_service_child(parent, &temporary, false, true)?;
    privilege.restore()?;
    drop(privilege);
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        rename_file(&file, parent, &destination, replace)
    })();
    if let Err(error) = result {
        // A failed rename may have published. Only remove the exact, still
        // temporary name; never delete a potentially published record.
        let unpublished = named_identity(parent, &temporary)
            .and_then(|identity| Ok(identity == Some(file_identity(&file)?)));
        if matches!(unpublished, Ok(true)) {
            delete_open_object(&file)?;
            drop(file);
            require_child_absent(parent, &temporary, false)?;
        }
        return Err(Error::message(format!(
            "Service record write failed ({error}); inspect or recover before retrying"
        )));
    }
    drop(file);
    Ok(())
}

fn current_prefix() -> Result<Option<(PathBuf, PathBuf)>> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let Some(version) = executable.parent() else {
        return Ok(None);
    };
    let Some(versions) = version.parent() else {
        return Ok(None);
    };
    let Some(prefix) = versions.parent() else {
        return Ok(None);
    };
    if executable.file_name().and_then(|name| name.to_str()) != Some("rdh.exe")
        || versions.file_name().and_then(|name| name.to_str()) != Some("versions")
        || !exists(&prefix.join(MANIFEST))?
    {
        return Ok(None);
    }
    Ok(Some((prefix.into(), executable)))
}

pub(crate) struct Lease {
    prefix: PathBuf,
    executable: PathBuf,
    lock: Object,
    pub record: Record,
    exclusive: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut overlap = OVERLAPPED::default();
        unsafe {
            UnlockFileEx(self.lock.file.as_raw_handle(), 0, 1, 0, &mut overlap);
        }
    }
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
        if exclusive && !administrator() {
            return Err(Error::message(
                "Service updates require an elevated administrator",
            ));
        }
        let (parents, name) = open_service_install_parent(&prefix.join(LOCK))?;
        check_service_object(parents.last().unwrap(), true)?;
        let file = open_child_request(
            parents.last().unwrap(),
            &name,
            false,
            GENERIC_READ | READ_CONTROL | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?;
        check_service_object(&file, true)?;
        if file.metadata()?.len() != 0 {
            return Err(invalid());
        }
        let mut overlap = OVERLAPPED::default();
        let flags = LOCKFILE_FAIL_IMMEDIATELY
            | if exclusive {
                LOCKFILE_EXCLUSIVE_LOCK
            } else {
                0
            };
        if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlap) } == 0 {
            let error = io::Error::last_os_error();
            return Err(
                if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                    Error::message(
                        "A service Agent or update controller is active; stop the Agent before updating",
                    )
                } else {
                    error.into()
                },
            );
        }
        // Closing this handle releases the lock if a subsequent read fails.
        let record: Record = read_record(&prefix, MANIFEST)?;
        record.validate()?;
        if !allow_pending && exists(&prefix.join(PENDING))? {
            return Err(Error::message(
                "Interrupted service update; stop the service and run upgrade --recover",
            ));
        }
        let source = std::iter::once(&record.current)
            .chain(record.previous.iter())
            .find(|version| binary(&prefix, version) == executable)
            .ok_or_else(|| {
                Error::message(
                    "Run the service manager from its current or retained previous executable",
                )
            })?;
        let _source = verify_binary(&prefix, source)?;
        let lease = Self {
            prefix,
            executable,
            lock: Object {
                file,
                _parents: parents,
            },
            record,
            exclusive,
        };
        if exclusive {
            Control::open(&lease.record.service, true)?.stopped()?;
        }
        Ok(lease)
    }
    pub fn exclusive(self, allow_pending: bool) -> Result<Self> {
        let prefix = self.prefix.clone();
        let executable = self.executable.clone();
        let expected = self.record.clone();
        drop(self);
        let result = Self::acquire(prefix, executable, true, allow_pending)?;
        if result.record != expected {
            return Err(Error::message(
                "Service installation changed while checking the release; retry after inspection",
            ));
        }
        Ok(result)
    }
    fn require_exclusive(&self) -> Result<()> {
        if !self.exclusive {
            return Err(Error::message(
                "An exclusive service manager lock is required",
            ));
        }
        Ok(())
    }
    pub fn stage_and_switch(&self, version: &str, bytes: &[u8], digest: &str) -> Result<()> {
        self.require_exclusive()?;
        let target = Version {
            version: version.into(),
            sha256: digest.into(),
            bytes: bytes.len() as u64,
        };
        target.validate()?;
        native_install::check_executable(bytes)?;
        if sha256_hex(bytes) != digest || version == self.record.current.version {
            return Err(Error::message("Invalid service update candidate"));
        }
        let target_path = binary(&self.prefix, &target);
        if !exists(target_path.parent().unwrap())? {
            let temporary_path = self
                .prefix
                .join("versions")
                .join(format!(".stage-{}", random::hex_token::<16>()?));
            let mut privilege = Privilege::owner_assignment()?;
            let mut creation = Creation::default();
            let directory = creation.root(&temporary_path, true)?;
            let candidate = creation.file(directory, "rdh.exe", bytes, true)?;
            privilege.restore()?;
            drop(privilege);
            creation.pin_directories_for_execution()?;
            let pinned_candidate = open_child(
                creation.objects[directory].directory.as_ref().unwrap(),
                &"rdh.exe".encode_utf16().collect::<Vec<_>>(),
                false,
                true,
            )?;
            check_service_object(&pinned_candidate, true)?;
            crate::processes::check_native_program(
                &candidate,
                &["--version"],
                version,
                false,
                Duration::from_secs(15),
            )?;
            drop(pinned_candidate);
            let (parents, name) = open_service_install_parent(target_path.parent().unwrap())?;
            check_service_object(parents.last().unwrap(), true)?;
            creation.publish_directory(directory, parents.last().unwrap(), &name)?;
        }
        let _target = verify_binary(&self.prefix, &target)?;
        crate::processes::check_native_program(
            &target_path,
            &["--version"],
            version,
            false,
            Duration::from_secs(15),
        )?;
        self.switch(self.record.with_version(target))
    }
    pub fn rollback(&self) -> Result<()> {
        let previous = self
            .record
            .previous
            .clone()
            .ok_or_else(|| Error::message("No previous service version is retained"))?;
        self.switch(self.record.with_version(previous))
    }
    fn switch(&self, next: Record) -> Result<()> {
        self.require_exclusive()?;
        if exists(&self.prefix.join(PENDING))? {
            return Err(Error::message("Recover the pending service update first"));
        }
        let old: Record = read_record(&self.prefix, MANIFEST)?;
        if old != self.record {
            return Err(Error::message("Service installation changed"));
        }
        let _old = verify_binary(&self.prefix, &old.current)?;
        let _next = verify_binary(&self.prefix, &next.current)?;
        let service = Control::open(&old.service, true)?;
        service.stopped()?;
        let before = service.configuration()?;
        before.matches(&old, &self.prefix)?;
        let pending = Pending {
            schema: 1,
            old,
            next,
            before,
        };
        pending.validate(&self.prefix)?;
        write_record(&self.prefix, PENDING, &pending, false)?;
        let result = (|| {
            service.replace(&pending.before, &pending.after(&self.prefix)?)?;
            write_record(&self.prefix, MANIFEST, &pending.next, true)?;
            service.stopped()?;
            if service.configuration()? != pending.after(&self.prefix)?
                || read_record::<Record>(&self.prefix, MANIFEST)? != pending.next
            {
                return Err(Error::message("Service changed during the update"));
            }
            remove_pending(&self.prefix, &pending)
        })();
        result.map_err(|error| Error::message(format!("Service update requires recovery: {error}. Stop the service and run upgrade --recover")))
    }
    pub fn recover(&self) -> Result<()> {
        self.require_exclusive()?;
        let pending: Pending = read_record(&self.prefix, PENDING)?;
        pending.validate(&self.prefix)?;
        let actual: Record = read_record(&self.prefix, MANIFEST)?;
        if actual != pending.old && actual != pending.next {
            return Err(Error::message(
                "Recovery refused: service installation was changed independently",
            ));
        }
        let _old = verify_binary(&self.prefix, &pending.old.current)?;
        let _next = verify_binary(&self.prefix, &pending.next.current)?;
        let service = Control::open(&pending.old.service, true)?;
        service.stopped()?;
        let actual_service = service.configuration()?;
        let after = pending.after(&self.prefix)?;
        if actual_service != pending.before && actual_service != after {
            return Err(Error::message(
                "Recovery refused: the service registration was changed independently",
            ));
        }
        if actual_service != pending.before {
            service.replace(&after, &pending.before)?;
        }
        write_record(&self.prefix, MANIFEST, &pending.old, true)?;
        service.stopped()?;
        if service.configuration()? != pending.before
            || read_record::<Record>(&self.prefix, MANIFEST)? != pending.old
        {
            return Err(Error::message("Service changed during recovery"));
        }
        remove_pending(&self.prefix, &pending)
    }
}

pub(crate) fn runtime_guard(
    args: &LaunchArgs,
    state_directory: Option<&Path>,
) -> Result<Option<Lease>> {
    let Some(lease) = Lease::acquire_current(false, false)? else {
        return Ok(None);
    };
    let record = &lease.record;
    if current_sid_text()? != record.account
        || lease.executable != record.binary(&lease.prefix)
        || args.pair_only
        || !args.roots.is_empty()
        || args.name.is_some()
        || !same_hub(&args.server, &record.server)
        || args
            .config
            .as_deref()
            .map(Path::new)
            .map(Path::canonicalize)
            .transpose()?
            .as_ref()
            != Some(&record.config)
        || state_directory
            .map(Path::canonicalize)
            .transpose()?
            .as_deref()
            != record.config.parent()
    {
        return Err(Error::message(
            "Service launch does not match the installed account, version, configuration, or state directory",
        ));
    }
    Ok(Some(lease))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ServiceConfig {
    service_type: u32,
    start_type: u32,
    error_control: u32,
    binary_path: String,
    load_order_group: String,
    tag_id: u32,
    dependencies: Vec<String>,
    account: String,
    display_name: String,
}
impl ServiceConfig {
    fn matches(&self, record: &Record, prefix: &Path) -> Result<()> {
        if self.service_type != SERVICE_WIN32_OWN_PROCESS
            || ![SERVICE_AUTO_START, SERVICE_DEMAND_START, SERVICE_DISABLED]
                .contains(&self.start_type)
            || !account_matches_sid(&self.account, &record.account)?
            || self.binary_path != record.command(prefix)?
        {
            return Err(Error::message(
                "The service registration does not match this native installation",
            ));
        }
        Ok(())
    }
}
struct Control(Service);
impl Control {
    fn open(name: &str, change: bool) -> Result<Self> {
        if !valid_name(name) {
            return Err(invalid());
        }
        let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
        if manager.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let manager = Service(manager);
        let name = wide(name);
        let raw = unsafe {
            OpenServiceW(
                manager.0,
                name.as_ptr(),
                SERVICE_QUERY_STATUS
                    | SERVICE_QUERY_CONFIG
                    | if change { SERVICE_CHANGE_CONFIG } else { 0 },
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(Service(raw)))
    }
    fn stopped(&self) -> Result<()> {
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut bytes = 0;
        if unsafe {
            QueryServiceStatusEx(
                self.0.0,
                SC_STATUS_PROCESS_INFO,
                (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut bytes,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if status.dwCurrentState != SERVICE_STOPPED || status.dwProcessId != 0 {
            return Err(Error::message(
                "Stop the registered service and wait for its process to exit before updating",
            ));
        }
        Ok(())
    }
    fn configuration(&self) -> Result<ServiceConfig> {
        let mut needed = 0;
        if unsafe { QueryServiceConfigW(self.0.0, null_mut(), 0, &mut needed) } != 0
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || needed < size_of::<QUERY_SERVICE_CONFIGW>() as u32
            || needed > 8192
        {
            return Err(Error::message(
                "Cannot determine the bounded service configuration size",
            ));
        }
        let size = needed as usize;
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        let pointer = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
        if unsafe { QueryServiceConfigW(self.0.0, pointer, needed, &mut needed) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let base = buffer.as_ptr() as usize;
        let end = base.checked_add(size).ok_or_else(invalid)?;
        let string = |pointer: *const u16| -> Result<(String, *const u16)> {
            if pointer.is_null() {
                return Ok((String::new(), pointer));
            }
            let start = pointer as usize;
            if start < base + size_of::<QUERY_SERVICE_CONFIGW>()
                || start >= end
                || !start.is_multiple_of(2)
            {
                return Err(invalid());
            }
            let units = unsafe { std::slice::from_raw_parts(pointer, (end - start) / 2) };
            let length = units
                .iter()
                .position(|unit| *unit == 0)
                .ok_or_else(invalid)?;
            let text = String::from_utf16(&units[..length]).map_err(|_| invalid())?;
            Ok((text, unsafe { pointer.add(length + 1) }))
        };
        let raw = unsafe { &*pointer };
        let mut dependencies = Vec::new();
        let mut dependency = raw.lpDependencies.cast_const();
        loop {
            let (text, next) = string(dependency)?;
            if text.is_empty() {
                break;
            }
            dependencies.push(text);
            dependency = next;
        }
        Ok(ServiceConfig {
            service_type: raw.dwServiceType,
            start_type: raw.dwStartType,
            error_control: raw.dwErrorControl,
            binary_path: string(raw.lpBinaryPathName)?.0,
            load_order_group: string(raw.lpLoadOrderGroup)?.0,
            tag_id: raw.dwTagId,
            dependencies,
            account: string(raw.lpServiceStartName)?.0,
            display_name: string(raw.lpDisplayName)?.0,
        })
    }
    fn replace(&self, expected: &ServiceConfig, next: &ServiceConfig) -> Result<()> {
        self.stopped()?;
        if self.configuration()? != *expected {
            return Err(Error::message(
                "Service registration changed before switching",
            ));
        }
        let mut allowed = expected.clone();
        allowed.binary_path = next.binary_path.clone();
        if allowed != *next {
            return Err(invalid());
        }
        let command = wide(&next.binary_path);
        // Change only ImagePath. Keep account, startup mode, dependencies,
        // error policy, display name and all optional SCM settings untouched.
        if unsafe {
            ChangeServiceConfigW(
                self.0.0,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                command.as_ptr(),
                null(),
                null_mut(),
                null(),
                null(),
                null(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        self.stopped()?;
        if self.configuration()? != *next {
            return Err(Error::message(
                "Service registration readback differs after switching",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Pending {
    schema: u32,
    old: Record,
    next: Record,
    before: ServiceConfig,
}
impl Pending {
    fn validate(&self, prefix: &Path) -> Result<()> {
        self.old.validate()?;
        self.next.validate()?;
        if self.schema != 1
            || self.old.current.version == self.next.current.version
            || self.old.with_version(self.next.current.clone()) != self.next
        {
            return Err(invalid());
        }
        self.before.matches(&self.old, prefix)?;
        self.after(prefix)?.matches(&self.next, prefix)
    }
    fn after(&self, prefix: &Path) -> Result<ServiceConfig> {
        let mut next = self.before.clone();
        next.binary_path = self.next.command(prefix)?;
        Ok(next)
    }
}
fn remove_pending(prefix: &Path, expected: &Pending) -> Result<()> {
    let (parents, name) = open_service_install_parent(&prefix.join(PENDING))?;
    let parent = parents.last().unwrap();
    let mut file = open_child_request(
        parent,
        &name,
        false,
        GENERIC_READ | DELETE | READ_CONTROL | SYNCHRONIZE,
        FILE_SHARE_READ,
    )?;
    check_service_object(&file, true)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(RECORD_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > RECORD_LIMIT || serde_json::from_slice::<Pending>(&bytes)? != *expected {
        return Err(Error::message(
            "Pending service update changed; retaining it for inspection",
        ));
    }
    delete_open_object(&file)?;
    drop(file);
    require_child_absent(parent, &name, false)
}
