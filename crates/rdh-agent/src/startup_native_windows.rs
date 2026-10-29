//! Windows future-startup settings. Enabling never starts or stops an Agent.
use crate::{
    config::DeviceConfig,
    error::{Error, Result},
    processes::windows_argument,
    startup::{Binding, Mode, StartupBackend, valid_name},
};
use std::{
    io,
    mem::size_of,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::ERROR_INSUFFICIENT_BUFFER,
    Security::{AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY},
    System::Services::{
        ChangeServiceConfigW, CloseServiceHandle, OpenSCManagerW, OpenServiceW,
        QUERY_SERVICE_CONFIGW, QueryServiceConfigW, SC_HANDLE, SC_MANAGER_CONNECT,
        SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_DISABLED, SERVICE_NO_CHANGE,
        SERVICE_QUERY_CONFIG,
    },
};

pub(crate) fn administrator() -> bool {
    let mut sid = null_mut();
    // SAFETY: the fixed authority/subauthorities identify Builtin Administrators.
    if unsafe {
        AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            32,
            544,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut sid,
        )
    } == 0
    {
        return false;
    }
    let mut member = 0;
    // SAFETY: null token checks the effective caller token, including UAC restrictions.
    let checked = unsafe { CheckTokenMembership(null_mut(), sid, &mut member) };
    unsafe { FreeSid(sid) };
    checked != 0 && member != 0
}

struct Service(SC_HANDLE);
impl Drop for Service {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}
impl Service {
    fn open(name: &str, write: bool) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::message("Invalid service name"));
        }
        let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
        if manager.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let manager = Self(manager);
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let service = unsafe {
            OpenServiceW(
                manager.0,
                name.as_ptr(),
                SERVICE_QUERY_CONFIG | if write { SERVICE_CHANGE_CONFIG } else { 0 },
            )
        };
        if service.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(service))
    }
    fn enabled(&self) -> Result<bool> {
        let mut bytes = 0;
        unsafe { QueryServiceConfigW(self.0, null_mut(), 0, &mut bytes) };
        if io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || bytes < size_of::<QUERY_SERVICE_CONFIGW>() as u32
            || bytes > 65536
        {
            return Err(Error::message("Cannot read service configuration size"));
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if unsafe { QueryServiceConfigW(self.0, buffer.as_mut_ptr().cast(), bytes, &mut bytes) }
            == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let configuration = unsafe { &*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
        Ok(configuration.dwStartType == SERVICE_AUTO_START)
    }
    fn set(&self, enabled: bool) -> Result<()> {
        // Only future start type changes. No running-service control is sent.
        if unsafe {
            ChangeServiceConfigW(
                self.0,
                SERVICE_NO_CHANGE,
                if enabled {
                    SERVICE_AUTO_START
                } else {
                    SERVICE_DISABLED
                },
                SERVICE_NO_CHANGE,
                null(),
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
        Ok(())
    }
}

#[path = "startup_tasks_windows.rs"]
mod tasks;

pub(crate) fn startup_path(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::message("Startup path is not Unicode"))?;
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        Ok(format!(r"\\{rest}"))
    } else {
        Ok(text.strip_prefix(r"\\?\").unwrap_or(text).into())
    }
}
fn task_error(error: windows::core::Error) -> Error {
    Error::message(format!(
        "Windows Task Scheduler failed (HRESULT=0x{:08x})",
        error.code().0 as u32
    ))
}

pub(crate) struct NativeStartup {
    config: PathBuf,
    executable: PathBuf,
}
impl NativeStartup {
    pub(crate) fn new(config: PathBuf) -> Result<Self> {
        let executable = std::env::current_exe()?.canonicalize()?;
        for path in [&config, &executable] {
            if !path.is_absolute()
                || path
                    .to_str()
                    .is_none_or(|text| text.chars().any(char::is_control))
            {
                return Err(Error::message(
                    "Startup requires absolute Unicode paths without control characters",
                ));
            }
        }
        Ok(Self { config, executable })
    }
    fn action_arguments(&self, server: &str) -> Result<String> {
        let config = startup_path(&self.config)?;
        Ok([
            "--config",
            &config,
            "--server",
            server,
            "--no-browser",
            "--recover-lock",
        ]
        .into_iter()
        .map(windows_argument)
        .collect::<Vec<_>>()
        .join(" "))
    }
    fn create_named_user(&self, requested_name: Option<&str>) -> Result<Binding> {
        if requested_name.is_some_and(|name| !valid_name(name)) {
            return Err(Error::message("Invalid startup task name"));
        }
        let config = DeviceConfig::read(&self.config)?;
        if !self.executable.is_file() {
            return Err(Error::message("A persistent Agent executable is required"));
        }
        let sid = crate::storage::windows::current_sid_text()?;
        let name = requested_name
            .map(str::to_owned)
            .unwrap_or_else(|| format!("RemoteDeviceHub-{sid}"));
        let arguments = self.action_arguments(&config.gateway_url)?;
        let directory = startup_path(
            self.config
                .parent()
                .ok_or_else(|| Error::message("Invalid config parent"))?,
        )?;
        let enabled = tasks::create(
            &name,
            &sid,
            &startup_path(&self.executable)?,
            &arguments,
            &directory,
        )
        .map_err(task_error)?;
        if enabled {
            return Err(Error::message(
                "New task was not confirmed disabled; inspect before retrying",
            ));
        }
        Ok(Binding {
            mode: Mode::User,
            name,
        })
    }
}
impl StartupBackend for NativeStartup {
    fn enabled(&self, binding: &Binding) -> Result<bool> {
        if !valid_name(&binding.name) {
            return Err(Error::message("Invalid startup binding"));
        }
        match binding.mode {
            Mode::Service => Service::open(&binding.name, false)?.enabled(),
            Mode::User => tasks::enabled(&binding.name).map_err(task_error),
            Mode::None => Err(Error::message("A native startup binding is required")),
        }
    }
    fn set(&self, binding: &Binding, enabled: bool) -> Result<()> {
        if !valid_name(&binding.name) {
            return Err(Error::message("Invalid startup binding"));
        }
        let actual = match binding.mode {
            Mode::Service => {
                let service = Service::open(&binding.name, true)?;
                service.set(enabled)?;
                service.enabled()?
            }
            Mode::User => tasks::set(&binding.name, enabled).map_err(task_error)?,
            Mode::None => return Err(Error::message("A native startup binding is required")),
        };
        if actual != enabled {
            return Err(Error::message(
                "Startup manager did not confirm the requested state; inspect before retrying",
            ));
        }
        Ok(())
    }
    fn create_user(&self) -> Result<Binding> {
        self.create_named_user(None)
    }
    fn service_authority(&self) -> bool {
        administrator()
    }
}

#[cfg(feature = "experimental-windows-install")]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateTask {
    pub config: PathBuf,
    pub name: String,
    pub sid: String,
    pub arguments: String,
    pub directory: String,
    pub old_executable: String,
    pub new_executable: String,
    pub fingerprint: String,
}

#[cfg(feature = "experimental-windows-install")]
fn expected_task(config: &Path, name: &str, old: &Path, new: &Path) -> Result<UpdateTask> {
    if !valid_name(name) || !config.is_absolute() {
        return Err(Error::message("Invalid managed task binding"));
    }
    let backend = NativeStartup {
        config: config.into(),
        executable: old.into(),
    };
    let paired = DeviceConfig::read(config)?;
    Ok(UpdateTask {
        config: config.into(),
        name: name.into(),
        sid: crate::storage::windows::current_sid_text()?,
        arguments: backend.action_arguments(&paired.gateway_url)?,
        directory: startup_path(
            config
                .parent()
                .ok_or_else(|| Error::message("Invalid task config directory"))?,
        )?,
        old_executable: startup_path(old)?,
        new_executable: startup_path(new)?,
        fingerprint: String::new(),
    })
}

#[cfg(feature = "experimental-windows-install")]
pub(crate) fn prepare_update_task(
    config: &Path,
    binding: &Binding,
    old: &Path,
    new: &Path,
) -> Result<UpdateTask> {
    if binding.mode != Mode::User {
        return Err(Error::message(
            "This installation has a service binding; coordinated Windows service target updates are not yet available",
        ));
    }
    let mut target = expected_task(config, &binding.name, old, new)?;
    let (executable, fingerprint) = tasks::target_snapshot(
        &target.name,
        &target.sid,
        &target.arguments,
        &target.directory,
    )
    .map_err(task_error)?;
    if executable != target.old_executable {
        return Err(Error::message(
            "Startup task does not execute this installation's current native version",
        ));
    }
    target.fingerprint = fingerprint;
    Ok(target)
}

#[cfg(feature = "experimental-windows-install")]
pub(crate) fn verify_update_task(target: &UpdateTask, old: &Path, new: &Path) -> Result<()> {
    let expected = expected_task(&target.config, &target.name, old, new)?;
    let (binding, _) = crate::startup::inspect_sidecar(&target.config)?;
    if binding
        != Some(Binding {
            mode: Mode::User,
            name: target.name.clone(),
        })
        || target.sid != expected.sid
        || target.arguments != expected.arguments
        || target.directory != expected.directory
        || target.old_executable != expected.old_executable
        || target.new_executable != expected.new_executable
        || target.fingerprint.len() != 64
        || !target
            .fingerprint
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(Error::message(
            "Interrupted startup target no longer matches the registered profile",
        ));
    }
    let (executable, fingerprint) = tasks::target_snapshot(
        &target.name,
        &target.sid,
        &target.arguments,
        &target.directory,
    )
    .map_err(task_error)?;
    if fingerprint != target.fingerprint
        || (executable != target.old_executable && executable != target.new_executable)
    {
        return Err(Error::message(
            "Startup task changed during the interrupted native update",
        ));
    }
    Ok(())
}

#[cfg(feature = "experimental-windows-install")]
pub(crate) fn switch_update_task(target: &UpdateTask, restore: bool) -> Result<()> {
    tasks::replace_target(
        target,
        if restore {
            &target.old_executable
        } else {
            &target.new_executable
        },
    )
    .map_err(task_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_service_is_an_error_instead_of_disabled() {
        assert!(
            Service::open(
                &format!("rdh-absent-{}", crate::random::hex_token::<12>().unwrap()),
                false
            )
            .is_err()
        );
    }
    #[test]
    fn task_paths_remove_only_the_canonical_extended_prefix() {
        assert_eq!(
            startup_path(Path::new(r"\\?\C:\private '😀\device.json")).unwrap(),
            r"C:\private '😀\device.json"
        );
        assert_eq!(
            startup_path(Path::new(r"\\?\UNC\server\share\device.json")).unwrap(),
            r"\\server\share\device.json"
        );
    }
    #[test]
    #[ignore = "registers and removes a uniquely named task and stopped service on an elevated Windows runner"]
    fn native_windows_task_and_service_startup_settings() {
        use crate::storage::windows::{create_child, open_parent};
        use windows_sys::Win32::System::Services::{
            CreateServiceW, DeleteService, QueryServiceStatus, SC_MANAGER_CREATE_SERVICE,
            SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS, SERVICE_STATUS,
            SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
        };
        assert!(
            administrator(),
            "Native startup fixture requires administrator authority"
        );
        let root = std::env::temp_dir().join(format!(
            "rdh startup '😀-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let (parents, name) = open_parent(&root, false, false).unwrap();
        drop(create_child(parents.last().unwrap(), &name, true).unwrap());
        drop(parents);
        let _directory = Directory(root.clone());
        let config_path = root.join("device '😀.json");
        let config = serde_json::json!({"gateway_url":"http://localhost:8111","device_id":"native-startup-fixture",
            "token":"x".repeat(32),"roots":crate::config::local_roots(&[format!("work={}",root.display())]).unwrap()});
        crate::storage::write_private_new(&config_path, &serde_json::to_vec(&config).unwrap())
            .unwrap();
        let backend = NativeStartup::new(config_path.canonicalize().unwrap()).unwrap();
        let name = format!(
            "RDH-Test-Startup-{}",
            crate::random::hex_token::<12>().unwrap()
        );
        struct Task {
            name: String,
            removed: bool,
        }
        impl Drop for Task {
            fn drop(&mut self) {
                if !self.removed {
                    let _ = tasks::remove(&self.name);
                }
            }
        }
        let binding = backend.create_named_user(Some(&name)).unwrap();
        let mut task = Task {
            name: name.clone(),
            removed: false,
        };
        let before = tasks::inspect(&name).unwrap();
        assert_eq!(
            before.executable,
            startup_path(&backend.executable).unwrap()
        );
        assert_eq!(
            before.arguments,
            backend.action_arguments("http://localhost:8111").unwrap()
        );
        assert_eq!(
            before.directory,
            startup_path(backend.config.parent().unwrap()).unwrap()
        );
        assert!(before.interactive && before.limited);
        assert!(
            crate::storage::windows::account_matches_sid(
                &before.trigger_user,
                &crate::storage::windows::current_sid_text().unwrap()
            )
            .unwrap()
        );
        assert!(
            !crate::storage::windows::account_matches_sid(&before.trigger_user, "S-1-5-18")
                .unwrap()
        );
        assert!(!backend.enabled(&binding).unwrap());
        assert!(backend.create_named_user(Some(&name)).is_err());
        for enabled in [true, false] {
            backend.set(&binding, enabled).unwrap();
            assert_eq!(backend.enabled(&binding).unwrap(), enabled);
            assert_eq!(tasks::inspect(&name).unwrap(), before);
        }
        tasks::remove(&name).unwrap();
        task.removed = true;
        assert!(backend.enabled(&binding).is_err());

        let manager = unsafe {
            OpenSCManagerW(
                null(),
                null(),
                SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
            )
        };
        assert!(!manager.is_null());
        let manager = Service(manager);
        let service_name = format!(
            "RDH-Test-Service-{}",
            crate::random::hex_token::<12>().unwrap()
        );
        let wide: Vec<u16> = service_name.encode_utf16().chain(Some(0)).collect();
        let binary: Vec<u16> = windows_argument(&startup_path(&backend.executable).unwrap())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let raw = unsafe {
            CreateServiceW(
                manager.0,
                wide.as_ptr(),
                wide.as_ptr(),
                SERVICE_QUERY_CONFIG | SERVICE_CHANGE_CONFIG | SERVICE_QUERY_STATUS | 0x0001_0000,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_DEMAND_START,
                SERVICE_ERROR_NORMAL,
                binary.as_ptr(),
                null(),
                null_mut(),
                null(),
                null(),
                null(),
            )
        };
        assert!(
            !raw.is_null(),
            "CreateService: {}",
            io::Error::last_os_error()
        );
        struct Registered(Service);
        impl Drop for Registered {
            fn drop(&mut self) {
                unsafe { DeleteService(self.0.0) };
            }
        }
        let registered = Registered(Service(raw));
        let binding = Binding {
            mode: Mode::Service,
            name: service_name,
        };
        for enabled in [true, false] {
            backend.set(&binding, enabled).unwrap();
            assert_eq!(backend.enabled(&binding).unwrap(), enabled);
            let mut state = SERVICE_STATUS::default();
            assert_ne!(unsafe { QueryServiceStatus(registered.0.0, &mut state) }, 0);
            assert_eq!(state.dwCurrentState, SERVICE_STOPPED);
        }
        drop(registered);
        assert!(Service::open(&binding.name, false).is_err());
        drop(task);
        drop(backend);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
