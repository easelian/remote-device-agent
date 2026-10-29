//! Create an immutable administrator-owned program tree and separate
//! LocalService-owned data. Existing services and directories are never replaced.
use std::{
    fs::File,
    io::{self, Read, Write},
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::Duration,
};

use serde_json::json;
use windows_sys::Win32::{
    Foundation::{
        ERROR_NOT_ALL_ASSIGNED, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SUCCESS, GetLastError,
        SetLastError,
    },
    Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    },
    Storage::FileSystem::{
        DELETE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, SYNCHRONIZE,
    },
    System::{
        Services::{
            CloseServiceHandle, CreateServiceW, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
            SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE, SC_STATUS_PROCESS_INFO,
            SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS,
            SERVICE_STATUS_PROCESS, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

use crate::{
    config::{DeviceConfig, sha256_hex},
    error::{Error, Result},
    policy,
    profile_lock::ProfileLocks,
    startup::{self, valid_name},
    startup_native::windows::{administrator, startup_path},
    storage::{
        self,
        windows::{
            FileIdentity, check_service_object, create_service_child, delete_open_object,
            file_identity, open_child_request, open_parent, open_service_install_parent,
            require_child_absent,
        },
    },
};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct Privilege {
    token: OwnedHandle,
    previous: TOKEN_PRIVILEGES,
    restored: bool,
}
impl Privilege {
    fn owner_assignment() -> Result<Self> {
        let mut raw = null_mut();
        // This command is synchronous and creates no runtime or worker threads.
        // Restore the caller's original privilege state on every return path.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut raw,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let name = wide("SeRestorePrivilege");
        let mut requested = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            ..TOKEN_PRIVILEGES::default()
        };
        if unsafe {
            LookupPrivilegeValueW(null(), name.as_ptr(), &mut requested.Privileges[0].Luid)
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        requested.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut length = 0;
        unsafe {
            SetLastError(ERROR_SUCCESS);
        }
        if unsafe {
            AdjustTokenPrivileges(
                token.as_raw_handle(),
                0,
                &requested,
                size_of::<TOKEN_PRIVILEGES>() as u32,
                &mut previous,
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(Error::message(
                "Service installation requires the administrator's restore privilege",
            ));
        }
        Ok(Self {
            token,
            previous,
            restored: false,
        })
    }
    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        unsafe { SetLastError(ERROR_SUCCESS) };
        if unsafe {
            AdjustTokenPrivileges(
                self.token.as_raw_handle(),
                0,
                &self.previous,
                0,
                null_mut(),
                null_mut(),
            )
        } == 0
            || unsafe { GetLastError() } != ERROR_SUCCESS
        {
            return Err(Error::message(
                "Cannot restore installer privileges; no candidate program was started",
            ));
        }
        self.restored = true;
        Ok(())
    }
}
impl Drop for Privilege {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("{error}");
        }
    }
}

struct Service(SC_HANDLE);
impl Drop for Service {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

struct Created {
    path: PathBuf,
    identity: FileIdentity,
    directory: Option<File>,
    program: bool,
}
#[derive(Default)]
struct Creation {
    // Every pre-existing ancestor and newly created directory remains pinned.
    parents: Vec<File>,
    objects: Vec<Created>,
    keep: bool,
}
impl Creation {
    fn root(&mut self, path: &Path, program: bool) -> Result<usize> {
        let (parents, name) = open_service_install_parent(path)?;
        let file = create_service_child(parents.last().unwrap(), &name, true, program)?;
        self.parents.extend(parents);
        let identity = file_identity(&file)?;
        self.objects.push(Created {
            path: path.into(),
            identity,
            directory: Some(file),
            program,
        });
        Ok(self.objects.len() - 1)
    }
    fn directory(&mut self, parent: usize, name: &str, program: bool) -> Result<usize> {
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let object = &self.objects[parent];
        let file = create_service_child(
            object.directory.as_ref().unwrap(),
            &name_utf16,
            true,
            program,
        )?;
        let path = object.path.join(name);
        let identity = file_identity(&file)?;
        self.objects.push(Created {
            path,
            identity,
            directory: Some(file),
            program,
        });
        Ok(self.objects.len() - 1)
    }
    fn file(&mut self, parent: usize, name: &str, bytes: &[u8], program: bool) -> Result<PathBuf> {
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let object = &self.objects[parent];
        let mut file = create_service_child(
            object.directory.as_ref().unwrap(),
            &name_utf16,
            false,
            program,
        )?;
        let path = object.path.join(name);
        let identity = file_identity(&file)?;
        self.objects.push(Created {
            path: path.clone(),
            identity,
            directory: None,
            program,
        });
        // On failure the file is closed before Creation's identity-checked cleanup.
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(path)
    }
    fn remove_created(&mut self) -> Result<()> {
        for object in self.objects.iter_mut().rev() {
            if let Some(file) = object.directory.take() {
                if file_identity(&file)? != object.identity {
                    return Err(Error::message("Created service directory changed"));
                }
                // An extra file introduced after creation makes directory
                // deletion fail. Never recurse into such unexpected content.
                delete_open_object(&file)?;
                drop(file);
            } else {
                let (parents, name) = open_parent(&object.path, false, false)?;
                let parent = parents.last().unwrap();
                let file = open_child_request(
                    parent,
                    &name,
                    false,
                    DELETE | READ_CONTROL | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )?;
                check_service_object(&file, object.program)?;
                if file_identity(&file)? != object.identity {
                    return Err(Error::message("Created service file changed"));
                }
                delete_open_object(&file)?;
                drop(file);
                require_child_absent(parent, &name, false)?;
            }
        }
        Ok(())
    }
}
impl Drop for Creation {
    fn drop(&mut self) {
        if !self.keep
            && let Err(error) = self.remove_created()
        {
            eprintln!(
                "Service installation cleanup was incomplete: {error}; inspect the requested new directories"
            );
        }
    }
}

struct Options {
    prefix: PathBuf,
    data: PathBuf,
    config: PathBuf,
    name: String,
}
fn options(args: &[String]) -> Result<Options> {
    let (mut prefix, mut data, mut config, mut name) = (None, None, None, None);
    let mut items = args.iter();
    while let Some(flag) = items.next() {
        let slot = match flag.as_str() {
            "--prefix" => &mut prefix,
            "--data" => &mut data,
            "--config" => &mut config,
            "--name" => &mut name,
            _ => return Err(Error::message("Invalid service installation option")),
        };
        if slot.is_some() {
            return Err(Error::message("Duplicate service installation option"));
        }
        *slot = Some(
            items
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or_else(|| Error::message(format!("{flag} requires a value")))?
                .clone(),
        );
    }
    let required = |value: Option<String>| {
        value.ok_or_else(|| Error::message("Use install-service --prefix DIRECTORY --data DIRECTORY --config PROFILE --name NAME"))
    };
    let path = |value: Option<String>| -> Result<PathBuf> {
        let value = required(value)?;
        let path = PathBuf::from(&value);
        if !path.is_absolute() || value.chars().any(char::is_control) {
            return Err(Error::message(
                "Service installation requires absolute Unicode paths without control characters",
            ));
        }
        Ok(path)
    };
    let result = Options {
        prefix: path(prefix)?,
        data: path(data)?,
        config: path(config)?,
        name: required(name)?,
    };
    if !valid_name(&result.name) {
        return Err(Error::message("Invalid service registration name"));
    }
    Ok(result)
}

fn optional_private(path: &Path) -> Result<Option<Vec<u8>>> {
    match storage::read_private(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let options = options(args)?;
    if !administrator() {
        return Err(Error::message(
            "LocalService installation requires an elevated administrator",
        ));
    }
    let manager = unsafe {
        OpenSCManagerW(
            null(),
            null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        )
    };
    if manager.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    let manager = Service(manager);
    let service_name = wide(&options.name);
    let existing = unsafe { OpenServiceW(manager.0, service_name.as_ptr(), SERVICE_QUERY_STATUS) };
    if !existing.is_null() {
        let _existing = Service(existing);
        return Err(Error::message(
            "The service already exists; existing registrations are never replaced by install-service",
        ));
    }
    if io::Error::last_os_error().raw_os_error() != Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) {
        return Err(io::Error::last_os_error().into());
    }
    let config_path = options.config.canonicalize()?;
    let mut profile_locks =
        ProfileLocks::acquire(&[config_path.clone(), startup::sidecar_path(&config_path)])?;
    crate::app::validate_config(&[
        "--config".into(),
        config_path.to_string_lossy().into_owned(),
    ])?;
    let configuration = DeviceConfig::read(&config_path)?;
    let config_bytes = storage::read_private(&config_path)?;
    let policy_bytes = optional_private(&policy::sidecar_path(&config_path))?;
    let startup_bytes = startup::new_service_state(&config_path, &options.name)?;
    let mut binary = File::open(std::env::current_exe()?)?;
    if binary.metadata()?.len() > crate::update::MAX_BINARY_BYTES {
        return Err(Error::message("Service executable is too large"));
    }
    let mut binary_bytes = Vec::new();
    (&mut binary)
        .take(crate::update::MAX_BINARY_BYTES + 1)
        .read_to_end(&mut binary_bytes)?;
    if binary_bytes.is_empty() || binary_bytes.len() as u64 > crate::update::MAX_BINARY_BYTES {
        return Err(Error::message("Invalid service executable size"));
    }
    let version = env!("CARGO_PKG_VERSION");
    let mut privilege = Privilege::owner_assignment()?;
    let mut creation = Creation::default();
    let program = creation.root(&options.prefix, true)?;
    let versions = creation.directory(program, "versions", true)?;
    let release = creation.directory(versions, version, true)?;
    let executable = creation.file(release, "rdh.exe", &binary_bytes, true)?;
    let data = creation.root(&options.data, false)?;
    let config = creation.file(data, "agent.json", &config_bytes, false)?;
    if let Some(bytes) = policy_bytes {
        creation.file(data, "agent.policy.json", &bytes, false)?;
    }
    creation.file(data, "agent.json.startup.json", &startup_bytes, false)?;
    let executable = executable.canonicalize()?;
    let config = config.canonicalize()?;
    creation.file(
        program,
        ".service-install.json",
        &serde_json::to_vec(&json!({
            "schema":1, "kind":"native-agent-service", "version":version,
            "service":options.name, "account":"S-1-5-19", "config":config,
            "binary":{"sha256":sha256_hex(&binary_bytes),"bytes":binary_bytes.len()}
        }))?,
        true,
    )?;
    // Confirm restoration before any subprocess inherits this token. Drop is
    // only the best-effort fallback on earlier error paths.
    privilege.restore()?;
    drop(privilege);
    crate::processes::check_native_program(
        &executable,
        &["--version"],
        version,
        false,
        Duration::from_secs(15),
    )?;
    let executable_text = startup_path(&executable)?;
    let config_text = startup_path(&config)?;
    let state_text = startup_path(
        config
            .parent()
            .ok_or_else(|| Error::message("Invalid service data directory"))?,
    )?;
    let command = [
        executable_text.as_str(),
        "service-run",
        "--service-name",
        &options.name,
        "--config",
        &config_text,
        "--server",
        &configuration.gateway_url,
        "--state-directory",
        &state_text,
        "--recover-lock",
    ]
    .into_iter()
    .map(crate::processes::windows_argument)
    .collect::<Vec<_>>()
    .join(" ");
    let command = wide(&command);
    let account = wide(r"NT AUTHORITY\LocalService");
    let password = [0u16];
    let service = unsafe {
        CreateServiceW(
            manager.0,
            service_name.as_ptr(),
            service_name.as_ptr(),
            SERVICE_QUERY_STATUS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            command.as_ptr(),
            null(),
            null_mut(),
            null(),
            account.as_ptr(),
            password.as_ptr(),
        )
    };
    if service.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    let service = Service(service);
    // Registration now names these exact files. Any later error or concurrent
    // administrative action retains them; cleanup must not strand a service.
    creation.keep = true;
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut bytes = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut bytes,
        )
    } == 0
        || status.dwCurrentState != SERVICE_STOPPED
        || status.dwProcessId != 0
    {
        return Err(Error::message(
            "Service registration was created, but its stopped state could not be confirmed; inspect it before starting or retrying",
        ));
    }
    profile_locks.release().map_err(|error| Error::message(format!("Service registration was created, but the source profile lock was not released: {error}; inspect before starting")))?;
    println!(
        "Native LocalService installation created.\nService: {}\nProgram: {}\nConfiguration: {}\nThe service is stopped and configured for manual start. Validate its file-root access and the source profile's handoff before starting it.",
        options.name, executable_text, config_text
    );
    Ok(())
}
