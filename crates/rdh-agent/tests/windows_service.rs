//! Real, uniquely named Windows services with a loopback authenticated Hub.
#![cfg(all(windows, feature = "experimental-windows-startup"))]

#[path = "support/initial_binary.rs"]
mod initial_binary;

#[cfg(feature = "experimental-windows-install")]
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rdh_agent::{config::local_roots, random};
use serde_json::{Value, json};
use std::{
    fs, io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, GetLastError, LocalFree, SetLastError, WAIT_OBJECT_0,
    },
    Security::{
        AdjustTokenPrivileges,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        GetTokenInformation, LookupPrivilegeValueW, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Services::{
            CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
            QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
            SC_STATUS_PROCESS_INFO, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
            SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED,
            SERVICE_WIN32_OWN_PROCESS, StartServiceW,
        },
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
};

#[cfg(feature = "experimental-windows-install")]
#[path = "support/native_candidate.rs"]
mod native_candidate;

const TOKEN: &str = "native-service-fixture-token-only";
fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

struct Privilege {
    token: OwnedHandle,
    previous: TOKEN_PRIVILEGES,
}
impl Privilege {
    fn restore_owner() -> Self {
        Self::enable("SeRestorePrivilege")
    }
    fn inspect_service_processes() -> Self {
        Self::enable("SeDebugPrivilege")
    }
    fn enable(name: &str) -> Self {
        let name = wide(name.as_ref());
        let mut raw = null_mut();
        assert_ne!(
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut raw,
                )
            },
            0
        );
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut next = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            ..TOKEN_PRIVILEGES::default()
        };
        assert_ne!(
            unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut next.Privileges[0].Luid) },
            0
        );
        next.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut length = 0;
        unsafe {
            SetLastError(ERROR_SUCCESS);
        }
        assert_ne!(
            unsafe {
                AdjustTokenPrivileges(
                    token.as_raw_handle(),
                    0,
                    &next,
                    size_of::<TOKEN_PRIVILEGES>() as u32,
                    &mut previous,
                    &mut length,
                )
            },
            0
        );
        assert_ne!(
            unsafe { GetLastError() },
            ERROR_NOT_ALL_ASSIGNED,
            "Native service fixture requires an elevated runner"
        );
        Self { token, previous }
    }
}
impl Drop for Privilege {
    fn drop(&mut self) {
        unsafe {
            AdjustTokenPrivileges(
                self.token.as_raw_handle(),
                0,
                &self.previous,
                0,
                null_mut(),
                null_mut(),
            );
        }
    }
}

fn service_acl(path: &Path, directory: bool, sid: &str) {
    // Only fixture-owned paths are transferred to the account that will run
    // the service. No production profile or service account is changed.
    let inherited = if directory { "OICI" } else { "" };
    let system = if sid == "S-1-5-18" {
        String::new()
    } else {
        format!("(A;{inherited};FA;;;SY)")
    };
    let sddl = wide(
        format!("O:{sid}D:P(A;{inherited};FA;;;{sid})(A;{inherited};FA;;;BA){system}").as_ref(),
    );
    set_acl(path, &sddl);
}

fn program_acl(path: &Path, directory: bool) {
    // Mirror the installer boundary: only administrators and SYSTEM can
    // change program files. LocalService can read and execute them.
    let inherited = if directory { "OICI" } else { "" };
    let sddl = wide(
        format!("O:BAD:P(A;{inherited};FA;;;BA)(A;{inherited};FA;;;SY)(A;{inherited};FRFX;;;LS)")
            .as_ref(),
    );
    set_acl(path, &sddl);
}

fn set_acl(path: &Path, sddl: &[u16]) {
    let mut descriptor = null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        },
        0
    );
    let mut owner = null_mut();
    let mut acl = null_mut();
    let (mut defaulted, mut present) = (0, 0);
    assert_ne!(
        unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) },
        0
    );
    assert_ne!(
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) },
        0
    );
    assert_ne!(present, 0);
    let path = wide(path.as_os_str());
    let code = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            null_mut(),
            acl,
            null_mut(),
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    assert_eq!(
        code,
        ERROR_SUCCESS,
        "Set fixture ACL: {}",
        io::Error::from_raw_os_error(code as i32)
    );
}

struct Handle(SC_HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}
struct Fixture {
    directory: PathBuf,
    service: Handle,
    removed: bool,
}
impl Fixture {
    fn status(&self) -> SERVICE_STATUS_PROCESS {
        let mut value = SERVICE_STATUS_PROCESS::default();
        let mut bytes = 0;
        assert_ne!(
            unsafe {
                QueryServiceStatusEx(
                    self.service.0,
                    SC_STATUS_PROCESS_INFO,
                    (&mut value as *mut SERVICE_STATUS_PROCESS).cast(),
                    size_of::<SERVICE_STATUS_PROCESS>() as u32,
                    &mut bytes,
                )
            },
            0
        );
        value
    }
    async fn state(&self, wanted: u32) -> SERVICE_STATUS_PROCESS {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let value = self.status();
            if value.dwCurrentState == wanted {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "Service state {}, expected {wanted}, exit {}",
                value.dwCurrentState,
                value.dwWin32ExitCode
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    fn start(&self) {
        assert_ne!(
            unsafe { StartServiceW(self.service.0, 0, null()) },
            0,
            "StartService: {}",
            io::Error::last_os_error()
        );
    }
    fn stop(&self) {
        let mut status = SERVICE_STATUS::default();
        assert_ne!(
            unsafe { ControlService(self.service.0, SERVICE_CONTROL_STOP, &mut status) },
            0,
            "StopService: {}",
            io::Error::last_os_error()
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.removed {
            let mut status = SERVICE_STATUS::default();
            unsafe {
                ControlService(self.service.0, SERVICE_CONTROL_STOP, &mut status);
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while self.status().dwCurrentState != SERVICE_STOPPED && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            if self.status().dwCurrentState != SERVICE_STOPPED {
                eprintln!(
                    "Retaining service fixture with unconfirmed cleanup: {}",
                    self.directory.display()
                );
                return;
            }
            unsafe {
                DeleteService(self.service.0);
            }
        }
        if std::thread::panicking() {
            eprintln!(
                "Retaining failed service fixture for inspection: {}",
                self.directory.display()
            );
            return;
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct Peer {
    socket: WebSocketStream<TcpStream>,
    instance: Value,
    next: u64,
    receive_budget: Duration,
}
impl Peer {
    async fn connect(listener: &TcpListener) -> Self {
        Self::connect_with_budget(listener, Duration::from_secs(45), None).await
    }
    async fn connect_with_budget(
        listener: &TcpListener,
        budget: Duration,
        fixture: Option<&Fixture>,
    ) -> Self {
        let started = Instant::now();
        let (stream, _) = tokio::time::timeout(budget, async {
            loop {
                tokio::select! {
                    accepted = listener.accept() => break accepted,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {
                        if let Some(fixture) = fixture {
                            let status = fixture.status();
                            assert!(started.elapsed() < Duration::from_secs(5) || status.dwCurrentState != SERVICE_STOPPED,
                                "Service exited before connecting: {}", status.dwWin32ExitCode);
                        }
                    }
                }
            }
        }).await.expect("Service did not connect to the fixture Hub").unwrap();
        let socket = accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/agent/native-service");
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer {TOKEN}")
            );
            Ok(response)
        })
        .await
        .unwrap();
        let mut peer = Self {
            socket,
            instance: Value::Null,
            next: 0,
            receive_budget: if budget == Duration::from_secs(45) {
                Duration::from_secs(30)
            } else {
                budget
            },
        };
        let hello = peer.receive().await;
        assert_eq!(hello["type"], "hello");
        peer.instance = hello["info"]["capabilities"]["operation_receipts"]["instance_id"].clone();
        assert!(peer.instance.is_string());
        peer.socket
            .send(Message::Text(
                json!({"type":"ready","device_id":"native-service"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        peer
    }
    async fn receive(&mut self) -> Value {
        tokio::time::timeout(self.receive_budget, async {
            loop {
                match self.socket.next().await.unwrap().unwrap() {
                    Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                    Message::Ping(data) => self.socket.send(Message::Pong(data)).await.unwrap(),
                    Message::Pong(_) => {}
                    other => panic!("Unexpected service message {other:?}"),
                }
            }
        })
        .await
        .expect("Service RPC timed out")
    }
    async fn call(&mut self, operation: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next.to_string();
        self.socket
            .send(Message::Text(
                json!({"type":"request","id":id,"operation":operation,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        loop {
            let value = self.receive().await;
            if value["type"] == "heartbeat" {
                continue;
            }
            assert_eq!(value["type"], "result");
            assert_eq!(value["id"], id);
            assert!(value.get("error").is_none(), "{operation}: {value}");
            return value["result"].clone();
        }
    }
}
fn process(pid: u32) -> OwnedHandle {
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    assert!(
        !handle.is_null(),
        "Open fixture process: {}",
        io::Error::last_os_error()
    );
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

fn process_sid(process: &OwnedHandle) -> String {
    let mut raw = null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut raw) },
        0,
        "Open fixture process token: {}",
        io::Error::last_os_error()
    );
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut bytes = 0;
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes) };
    assert!((size_of::<TOKEN_USER>() as u32..=65536).contains(&bytes));
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        },
        0
    );
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    assert_ne!(
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) },
        0
    );
    let mut length = 0;
    while length < 256 && unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    assert!(length < 256);
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap();
    unsafe { LocalFree(text.cast()) };
    sid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "creates and removes one native LocalSystem service on an elevated Windows runner"]
async fn native_windows_service_connects_drains_jobs_restarts_and_refuses_missing_config() {
    service_lifecycle("S-1-5-18", None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "creates and removes one native LocalService service on an elevated Windows runner"]
async fn native_windows_local_service_preserves_account_identity_and_process_cleanup() {
    service_lifecycle("S-1-5-19", Some(r"NT AUTHORITY\LocalService")).await;
}

#[cfg(feature = "experimental-windows-install")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "installs and removes a native LocalService registration with separate protected program and data trees"]
async fn native_windows_installer_preserves_profile_and_runs_as_local_service() {
    use rdh_agent::{profile_lock::ProfileLocks, storage};
    use windows_sys::Win32::System::Services::OpenServiceW;
    let _inspection = Privilege::inspect_service_processes();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let name = format!("RDH-Test-Install-{}", random::hex_token::<12>().unwrap());
    let directory =
        PathBuf::from(std::env::var_os("ProgramData").unwrap()).join(format!("{name} '😀"));
    fs::create_dir(&directory).unwrap();
    let source = directory.join("source");
    let work = directory.join("work");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&work).unwrap();
    let sid = process_sid(&process(std::process::id()));
    {
        let _restore = Privilege::restore_owner();
        program_acl(&directory, true);
        service_acl(&source, true, &sid);
        service_acl(&work, true, "S-1-5-19");
    }
    let config_source = source.join("agent.json");
    let policy_source = source.join("agent.policy.json");
    let startup_source = source.join("agent.json.startup.json");
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let contents = serde_json::to_vec(
        &json!({"gateway_url":origin,"device_id":"native-service","token":TOKEN,
        "roots":local_roots(&[format!("work={}",work.display())]).unwrap(),
        "allow_write":true,"allow_shell":true,"shell":powershell}),
    )
    .unwrap();
    let policy = br#"{"version":1,"audit":{"enabled":true,"required":true}}"#;
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000;
    let startup = serde_json::to_vec(&json!({"version":1,"binding":null,
        "pending":{"request_id":"01234567-89ab-4def-8123-456789abcdef","mode":"user","expires_at":expires}})).unwrap();
    storage::write_private_new(&config_source, &contents).unwrap();
    storage::write_private_new(&policy_source, policy).unwrap();
    storage::write_private_new(&startup_source, &startup).unwrap();
    let program = directory.join("program");
    let data = directory.join("data");
    let install = || {
        std::process::Command::new(initial_binary::path())
            .args(["install-service", "--prefix"])
            .arg(&program)
            .arg("--data")
            .arg(&data)
            .arg("--config")
            .arg(&config_source)
            .args(["--name", &name])
            .env("PATH", "")
            .output()
            .unwrap()
    };
    let mut locked = ProfileLocks::acquire(std::slice::from_ref(&config_source)).unwrap();
    assert!(
        !install().status.success(),
        "Running/locked source profile must not be copied"
    );
    assert!(!program.exists() && !data.exists());
    locked.release().unwrap();
    fs::create_dir(&program).unwrap();
    let sentinel = program.join("existing.txt");
    fs::write(&sentinel, b"keep existing installation").unwrap();
    assert!(!install().status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep existing installation");
    assert!(!data.exists());
    fs::remove_file(&sentinel).unwrap();
    fs::remove_dir(&program).unwrap();
    let result = install();
    if !result.status.success() {
        // Read only ACL metadata for this unique fixture and its ancestors.
        let mut paths: Vec<_> = directory
            .ancestors()
            .take(16)
            .map(Path::to_path_buf)
            .collect();
        paths.extend([source.clone(), work.clone(), program.clone(), data.clone()]);
        let paths = paths
            .iter()
            .map(|path| format!("'{}'", path.to_str().unwrap().replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            "$ErrorActionPreference = 'Stop'; foreach ($path in @({paths})) {{ if (Test-Path -LiteralPath $path) {{ $acl = Get-Acl -LiteralPath $path; [Console]::WriteLine($path + ': ' + $acl.Sddl) }} }}"
        );
        let diagnostic = service_manager(
            &powershell,
            &["-NoProfile", "-NonInteractive", "-Command", &script],
        )
        .await;
        eprintln!(
            "Installer ACL metadata: {}\n{}",
            String::from_utf8_lossy(&diagnostic.stdout),
            String::from_utf8_lossy(&diagnostic.stderr)
        );
    }
    assert!(
        result.status.success(),
        "installer stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    assert!(!manager.is_null());
    let manager = Handle(manager);
    let service_name = wide(name.as_ref());
    let service = unsafe {
        OpenServiceW(
            manager.0,
            service_name.as_ptr(),
            SERVICE_QUERY_STATUS
                | SERVICE_START
                | SERVICE_STOP
                | 0x0001_0000
                | windows_sys::Win32::System::Services::SERVICE_QUERY_CONFIG
                | windows_sys::Win32::System::Services::SERVICE_CHANGE_CONFIG,
        )
    };
    assert!(!service.is_null());
    let fixture = Fixture {
        directory,
        service: Handle(service),
        removed: false,
    };
    assert_eq!(fixture.status().dwCurrentState, SERVICE_STOPPED);
    assert_eq!(fs::read(&config_source).unwrap(), contents);
    assert_eq!(fs::read(&policy_source).unwrap(), policy);
    assert_eq!(fs::read(&startup_source).unwrap(), startup);
    let config = data.join("agent.json");
    let copied_startup: Value =
        serde_json::from_slice(&fs::read(data.join("agent.json.startup.json")).unwrap()).unwrap();
    assert_eq!(
        copied_startup["binding"],
        json!({"mode":"service","name":name})
    );
    assert_eq!(
        copied_startup["pending"],
        serde_json::from_slice::<Value>(&startup).unwrap()["pending"]
    );
    assert_eq!(fs::read(&config).unwrap(), contents);
    assert_eq!(fs::read(data.join("agent.policy.json")).unwrap(), policy);
    assert!(
        !install().status.success(),
        "Duplicate installation must preserve the registered service"
    );
    assert_eq!(fixture.status().dwCurrentState, SERVICE_STOPPED);
    exercise_service_updates(&listener, &fixture, &program, &config, &contents).await;
    exercise_service(
        &listener,
        "S-1-5-19",
        fixture,
        config,
        program.join(".service-install.json"),
        contents,
        true,
    )
    .await;
}

#[cfg(feature = "experimental-windows-install")]
fn service_configuration(service: SC_HANDLE) -> Value {
    use windows_sys::Win32::System::Services::{QUERY_SERVICE_CONFIGW, QueryServiceConfigW};
    let mut size = 0;
    assert_eq!(
        unsafe { QueryServiceConfigW(service, null_mut(), 0, &mut size) },
        0
    );
    assert!((size_of::<QUERY_SERVICE_CONFIGW>() as u32..=8192).contains(&size));
    let mut memory = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    let capacity = size as usize;
    let pointer = memory.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    assert_ne!(
        unsafe { QueryServiceConfigW(service, pointer, size, &mut size) },
        0
    );
    let string = |pointer: *const u16| -> (String, *const u16) {
        if pointer.is_null() {
            return (String::new(), pointer);
        }
        let start = pointer as usize;
        let first = memory.as_ptr() as usize;
        let end = first + capacity;
        assert!(
            start >= first + size_of::<QUERY_SERVICE_CONFIGW>()
                && start < end
                && start.is_multiple_of(2)
        );
        let units = unsafe { std::slice::from_raw_parts(pointer, (end - start) / 2) };
        let length = units.iter().position(|unit| *unit == 0).unwrap();
        (String::from_utf16(&units[..length]).unwrap(), unsafe {
            pointer.add(length + 1)
        })
    };
    let data = unsafe { &*pointer };
    let mut dependency = data.lpDependencies.cast_const();
    let mut dependencies = Vec::new();
    loop {
        let (value, next) = string(dependency);
        if value.is_empty() {
            break;
        }
        dependencies.push(value);
        dependency = next;
    }
    json!({"service_type":data.dwServiceType,"start_type":data.dwStartType,
        "error_control":data.dwErrorControl,"binary_path":string(data.lpBinaryPathName).0,
        "load_order_group":string(data.lpLoadOrderGroup).0,"tag_id":data.dwTagId,
        "dependencies":dependencies,"account":string(data.lpServiceStartName).0,
        "display_name":string(data.lpDisplayName).0})
}

#[cfg(feature = "experimental-windows-install")]
fn set_service_command(service: SC_HANDLE, command: &str, display_name: Option<&str>) {
    use windows_sys::Win32::System::Services::{ChangeServiceConfigW, SERVICE_NO_CHANGE};
    let command = wide(command.as_ref());
    let display_name = display_name.map(|value| wide(value.as_ref()));
    assert_ne!(
        unsafe {
            ChangeServiceConfigW(
                service,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                command.as_ptr(),
                null(),
                null_mut(),
                null(),
                null(),
                null(),
                display_name.as_ref().map_or(null(), |value| value.as_ptr()),
            )
        },
        0
    );
}

#[cfg(feature = "experimental-windows-install")]
async fn service_manager(executable: &Path, args: &[&str]) -> std::process::Output {
    use std::process::Stdio;
    tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(executable)
            .args(args)
            .env("PATH", "")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("Service manager exceeded its fixture deadline")
    .unwrap()
}

#[cfg(feature = "experimental-windows-install")]
fn manager_success(result: std::process::Output) {
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(feature = "experimental-windows-install")]
fn fixture_program_record(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    let _restore = Privilege::restore_owner();
    program_acl(path, false);
}

#[cfg(feature = "experimental-windows-install")]
fn process_image(process: &OwnedHandle) -> PathBuf {
    use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;
    let mut image = vec![0u16; 32_768];
    let mut length = image.len() as u32;
    assert_ne!(
        unsafe {
            QueryFullProcessImageNameW(process.as_raw_handle(), 0, image.as_mut_ptr(), &mut length)
        },
        0
    );
    PathBuf::from(String::from_utf16(&image[..length as usize]).unwrap())
        .canonicalize()
        .unwrap()
}

#[cfg(feature = "experimental-windows-install")]
async fn stop_and_wait(fixture: &Fixture, owner: OwnedHandle) {
    fixture.stop();
    assert_eq!(fixture.state(SERVICE_STOPPED).await.dwWin32ExitCode, 0);
    assert_eq!(
        unsafe { WaitForSingleObject(owner.as_raw_handle(), 5000) },
        WAIT_OBJECT_0
    );
}

#[cfg(feature = "experimental-windows-install")]
async fn exercise_service_updates(
    listener: &TcpListener,
    fixture: &Fixture,
    program: &Path,
    config: &Path,
    contents: &[u8],
) {
    use rdh_agent::config::sha256_hex;
    let (new_version, candidate) = native_candidate::load();
    let old_version = env!("CARGO_PKG_VERSION");
    let executable = |version: &str| program.join("versions").join(version).join("rdh.exe");
    let original = executable(old_version);
    let updated = executable(&new_version);
    let entry = program.join("rdh.exe");
    for name in rdh_agent::native_launcher::COMMAND_NAMES {
        let output = service_manager(&program.join(format!("{name}.exe")), &["--version"]).await;
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), old_version);
        manager_success(output);
    }
    let manifest = program.join(".service-install.json");
    let pending = program.join(".service-update-pending.json");
    let current = || serde_json::from_slice::<Value>(&fs::read(&manifest).unwrap()).unwrap();
    let old = current();
    let before = service_configuration(fixture.service.0);
    let artifact = fixture.directory.join("update-candidate.exe");
    fs::write(&artifact, &candidate).unwrap();
    let platform = if cfg!(target_arch = "aarch64") {
        "win32-arm64"
    } else {
        "win32-x64"
    };
    let index = fixture.directory.join("channel.json");
    fs::write(
        &index,
        serde_json::to_vec(&json!({"schema":2,"kind":"native-agent-channel",
        "channel":"stable","version":new_version,"rollout":{"percent":100},
        "platforms":{platform:{"url":artifact.to_str().unwrap(),"format":"native-binary",
            "sha256":sha256_hex(&candidate),"bytes":candidate.len()}}}))
        .unwrap(),
    )
    .unwrap();
    let upgrade = [
        "upgrade",
        "--index",
        index.to_str().unwrap(),
        "--force",
        "--no-restart",
    ];
    manager_success(
        service_manager(
            &original,
            &[
                "upgrade",
                "--check",
                "--index",
                index.to_str().unwrap(),
                "--force",
            ],
        )
        .await,
    );
    assert_eq!(current(), old);
    fixture.start();
    let mut peer = Peer::connect(listener).await;
    let owner = process(fixture.state(SERVICE_RUNNING).await.dwProcessId);
    assert_eq!(process_sid(&owner), "S-1-5-19");
    assert_eq!(process_image(&owner), original.canonicalize().unwrap());
    peer.call(
        "write_file",
        json!({"root":"work","path":"upgrade.txt","content":"same device across service versions"}),
    )
    .await;
    let refused = service_manager(&original, &upgrade).await;
    assert!(
        !refused.status.success(),
        "An active service must exclude the update controller"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("active"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(current(), old);
    assert_eq!(service_configuration(fixture.service.0), before);
    stop_and_wait(fixture, owner).await;
    drop(peer);
    let mut bad_index: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    bad_index["platforms"][platform]["sha256"] = Value::String("0".repeat(64));
    let bad_index_path = fixture.directory.join("bad-channel.json");
    fs::write(&bad_index_path, serde_json::to_vec(&bad_index).unwrap()).unwrap();
    let refused = service_manager(
        &original,
        &[
            "upgrade",
            "--index",
            bad_index_path.to_str().unwrap(),
            "--force",
            "--no-restart",
        ],
    )
    .await;
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("Invalid service update candidate"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(current(), old);
    assert_eq!(service_configuration(fixture.service.0), before);
    assert!(!updated.exists() && !pending.exists());
    manager_success(service_manager(&entry, &upgrade).await);
    let selected = service_manager(&entry, &["--version"]).await;
    assert_eq!(
        String::from_utf8_lossy(&selected.stdout).trim(),
        new_version
    );
    manager_success(selected);
    let next = current();
    assert_eq!(next["current"]["version"], new_version);
    assert_eq!(next["previous"], old["current"]);
    for key in [
        "schema",
        "kind",
        "service",
        "account",
        "config",
        "server",
        "install_id",
    ] {
        assert_eq!(next[key], old[key]);
    }
    assert_eq!(fs::read(config).unwrap(), contents);
    let after = service_configuration(fixture.service.0);
    let mut unchanged = after.clone();
    unchanged["binary_path"] = before["binary_path"].clone();
    assert_eq!(
        unchanged, before,
        "Only the service executable path may change"
    );
    assert!(!pending.exists());
    fixture.start();
    let mut peer = Peer::connect(listener).await;
    let owner = process(fixture.state(SERVICE_RUNNING).await.dwProcessId);
    assert_eq!(process_sid(&owner), "S-1-5-19");
    assert_eq!(process_image(&owner), updated.canonicalize().unwrap());
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"upgrade.txt"}))
            .await["content"],
        "same device across service versions"
    );
    assert!(
        !service_manager(&updated, &["rollback", "--no-restart"])
            .await
            .status
            .success()
    );
    stop_and_wait(fixture, owner).await;
    drop(peer);
    manager_success(
        service_manager(
            &program.join("remote-device-agent.exe"),
            &["rollback", "--no-restart"],
        )
        .await,
    );
    let restored = current();
    assert_eq!(restored["current"], old["current"]);
    assert_eq!(restored["previous"], next["current"]);
    assert_eq!(service_configuration(fixture.service.0), before);
    fixture.start();
    let mut peer = Peer::connect(listener).await;
    let owner = process(fixture.state(SERVICE_RUNNING).await.dwProcessId);
    assert_eq!(process_image(&owner), original.canonicalize().unwrap());
    assert_eq!(process_sid(&owner), "S-1-5-19");
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"upgrade.txt"}))
            .await["content"],
        "same device across service versions"
    );
    stop_and_wait(fixture, owner).await;
    drop(peer);

    // Materialize each durable interruption boundary without adding a runtime
    // fault-injection switch: pending only, SCM changed, then manifest changed.
    let transaction = json!({"schema":1,"old":restored,"next":next,"before":before});
    for phase in 0..3 {
        fixture_program_record(&pending, &transaction);
        if phase >= 1 {
            set_service_command(
                fixture.service.0,
                after["binary_path"].as_str().unwrap(),
                None,
            );
        }
        if phase >= 2 {
            fixture_program_record(&manifest, &next);
        }
        let saved_manifest = fs::read(&manifest).unwrap();
        let saved_pending = fs::read(&pending).unwrap();
        assert!(!service_manager(&original, &upgrade).await.status.success());
        assert_eq!(fs::read(&manifest).unwrap(), saved_manifest);
        assert_eq!(fs::read(&pending).unwrap(), saved_pending);
        fixture.start();
        assert_ne!(fixture.state(SERVICE_STOPPED).await.dwWin32ExitCode, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err(),
            "An interrupted service update must not launch the Agent"
        );
        if phase == 1 {
            let command = after["binary_path"].as_str().unwrap();
            set_service_command(
                fixture.service.0,
                command,
                Some("Fixture administrator changed registration"),
            );
            let external = service_configuration(fixture.service.0);
            let refused = service_manager(&original, &["upgrade", "--recover"]).await;
            assert!(!refused.status.success());
            assert!(
                String::from_utf8_lossy(&refused.stderr).contains("changed independently"),
                "{}",
                String::from_utf8_lossy(&refused.stderr)
            );
            assert_eq!(service_configuration(fixture.service.0), external);
            assert_eq!(fs::read(&manifest).unwrap(), saved_manifest);
            assert_eq!(fs::read(&pending).unwrap(), saved_pending);
            set_service_command(
                fixture.service.0,
                command,
                Some(before["display_name"].as_str().unwrap()),
            );
        }
        manager_success(
            service_manager(
                &program.join("remote-device-hub.exe"),
                &["upgrade", "--recover"],
            )
            .await,
        );
        assert!(!pending.exists());
        assert_eq!(current(), restored);
        assert_eq!(service_configuration(fixture.service.0), before);
        assert_eq!(fs::read(config).unwrap(), contents);
    }
    // A controller can win the lock just before SCM starts the process.
    // The process must exit before connecting or executing any device work.
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
            LockFileEx, UnlockFileEx,
        };
        use windows_sys::Win32::System::IO::OVERLAPPED;
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(program.join(".service-manager.lock"))
            .unwrap();
        let mut overlap = OVERLAPPED::default();
        assert_ne!(
            unsafe {
                LockFileEx(
                    lock.as_raw_handle(),
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    0,
                    1,
                    0,
                    &mut overlap,
                )
            },
            0
        );
        fixture.start();
        assert_ne!(fixture.state(SERVICE_STOPPED).await.dwWin32ExitCode, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), listener.accept())
                .await
                .is_err()
        );
        assert_ne!(
            unsafe { UnlockFileEx(lock.as_raw_handle(), 0, 1, 0, &mut overlap) },
            0
        );
    }
}

async fn service_lifecycle(sid: &str, account: Option<&str>) {
    // Cross-account token inspection requires SeDebugPrivilege. It is scoped
    // to the elevated test driver; SCM still creates the service and its jobs
    // with the requested account's own token. The original state is restored.
    let _inspection_privilege = Privilege::inspect_service_processes();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let name = format!("RDH-Test-Native-{}", random::hex_token::<12>().unwrap());
    let directory =
        PathBuf::from(std::env::var_os("ProgramData").unwrap()).join(format!("{name} '😀"));
    fs::create_dir(&directory).unwrap();
    let data = directory.join("data");
    fs::create_dir(&data).unwrap();
    let work = data.join("work");
    fs::create_dir(&work).unwrap();
    let binary = directory.join("rdh.exe");
    fs::copy(initial_binary::path(), &binary).unwrap();
    // This non-executable file has the same ACL as the binary. Unlike a
    // running executable, image-section locking cannot explain write denial.
    let protected = directory.join("program-access.txt");
    fs::write(&protected, b"administrator-owned program data").unwrap();
    let config = data.join("device '😀.json");
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let contents = serde_json::to_vec(
        &json!({"gateway_url":origin,"device_id":"native-service","token":TOKEN,
        "roots":local_roots(&[format!("work={}",work.display())]).unwrap(),
        "allow_write":true,"allow_shell":true,"shell":powershell}),
    )
    .unwrap();
    fs::write(&config, &contents).unwrap();
    let policy = data.join("device '😀.policy.json");
    fs::write(&policy, br#"{"version":1,"audit":{"enabled":false}}"#).unwrap();
    {
        let _privilege = Privilege::restore_owner();
        for path in [&config, &policy] {
            service_acl(path, false, sid);
        }
        service_acl(&work, true, sid);
        service_acl(&data, true, sid);
        for path in [&binary, &protected] {
            program_acl(path, false);
        }
        program_acl(&directory, true);
    }
    let manager = unsafe {
        OpenSCManagerW(
            null(),
            null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        )
    };
    assert!(
        !manager.is_null(),
        "Native service fixture requires an elevated Windows runner"
    );
    let manager = Handle(manager);
    let command = format!(
        "\"{}\" service-run --service-name {} --config \"{}\" --server {} --recover-lock",
        binary.display(),
        name,
        config.display(),
        origin
    );
    let command = wide(command.as_ref());
    let service_name = wide(name.as_ref());
    let account = account.map(|account| wide(account.as_ref()));
    let password = [0u16];
    let raw = unsafe {
        CreateServiceW(
            manager.0,
            service_name.as_ptr(),
            service_name.as_ptr(),
            SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP | 0x0001_0000,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            command.as_ptr(),
            null(),
            null_mut(),
            null(),
            account.as_ref().map_or(null(), |name| name.as_ptr()),
            password.as_ptr(),
        )
    };
    assert!(
        !raw.is_null(),
        "Create fixture service: {}",
        io::Error::last_os_error()
    );
    let fixture = Fixture {
        directory,
        service: Handle(raw),
        removed: false,
    };
    exercise_service(&listener, sid, fixture, config, protected, contents, false).await;
}

async fn exercise_service(
    listener: &TcpListener,
    sid: &str,
    mut fixture: Fixture,
    config: PathBuf,
    protected: PathBuf,
    contents: Vec<u8>,
    required_service_audit: bool,
) {
    fixture.start();
    let mut peer = Peer::connect(listener).await;
    let running = fixture.state(SERVICE_RUNNING).await;
    let owner = process(running.dwProcessId);
    assert_eq!(
        process_sid(&owner),
        sid,
        "SCM must use the requested runtime account"
    );
    peer.call(
        "write_file",
        json!({"root":"work","path":"native.txt","content":"service 😀"}),
    )
    .await;
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"native.txt"}))
            .await["content"],
        "service 😀"
    );
    if required_service_audit {
        let state = config.parent().unwrap().join("remote-device-hub");
        let audit_directory = state.join("audit");
        let audit = audit_directory.join("native-service.jsonl");
        let quote = |path: &Path| path.to_str().unwrap().replace('\'', "''");
        let command = format!(
            "$ErrorActionPreference = 'Stop'; \
            $stream = [IO.File]::Open('{}', [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)); \
            $reader = [IO.StreamReader]::new($stream); \
            try {{ $text = $reader.ReadToEnd() }} finally {{ $reader.Dispose(); $stream.Dispose() }}; \
            if (!$text.Contains('\"op\":\"write_file\"') -or $text.Contains('{TOKEN}')) {{ exit 21 }}; \
            $admin = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'); \
            foreach ($path in @('{}', '{}', '{}')) {{ \
              $directory = [IO.Directory]::Exists($path); \
              $sections = [Security.AccessControl.AccessControlSections]::Access; \
              $acl = if ($directory) {{ [IO.Directory]::GetAccessControl($path, $sections) }} else {{ [IO.File]::GetAccessControl($path, $sections) }}; \
              $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($admin, 'FullControl', 'Allow')); \
              if ($directory) {{ [IO.Directory]::SetAccessControl($path, $acl) }} else {{ [IO.File]::SetAccessControl($path, $acl) }}; \
            }}; [Console]::Write('service-audit-ok')",
            quote(&audit),
            quote(&state),
            quote(&audit_directory),
            quote(&audit),
        );
        // Read the required audit as its real LocalService owner, then grant
        // the test administrator access to these three exact fixture objects
        // for post-stop cleanup. Only the modified DACL is persisted: the
        // PowerShell Set-Acl provider also requests SACL changes, which would
        // incorrectly require SeSecurityPrivilege from LocalService here.
        // No real service profile is touched.
        let started = peer
            .call(
                "start_process",
                json!({"root":"work","command":command,"wait_seconds":0}),
            )
            .await;
        let deadline = Instant::now() + Duration::from_secs(60);
        let result = loop {
            let value = peer
                .call("read_process", json!({"job_id":started["job_id"]}))
                .await;
            if value["status"] != "running" && value["cleanup_pending"] == false {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "Service audit probe did not finish: {value}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["output"], "service-audit-ok", "{result}");
    }
    if sid == "S-1-5-19" {
        let path = protected.to_str().unwrap().replace('\'', "''");
        let protected_bytes = fs::read(&protected).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&protected_bytes);
        let command = format!(
            "$ErrorActionPreference = 'Stop'; \
            if ([Convert]::ToBase64String([IO.File]::ReadAllBytes('{path}')) -ne '{encoded}') {{ exit 11 }}; \
            try {{ $file = [IO.File]::Open('{path}', [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite); $file.Dispose(); exit 12 }} \
            catch {{ if ($_.Exception.GetBaseException() -is [UnauthorizedAccessException]) {{ exit 0 }}; exit 13 }}"
        );
        let started = peer
            .call(
                "start_process",
                json!({"root":"work","command":command,"wait_seconds":0}),
            )
            .await;
        let deadline = Instant::now() + Duration::from_secs(30);
        let result = loop {
            let value = peer
                .call("read_process", json!({"job_id":started["job_id"]}))
                .await;
            if value["status"] != "running" && value["cleanup_pending"] == false {
                break value;
            }
            assert!(
                Instant::now() < deadline,
                "Program ACL check did not finish"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert_eq!(result["status"], "exited", "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(fs::read(&protected).unwrap(), protected_bytes);
    }
    let job = peer.call("start_process", json!({"root":"work","command":"[Console]::WriteLine($PID); [Console]::Out.Flush(); Start-Sleep -Seconds 120","wait_seconds":0})).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let child = loop {
        let value = peer
            .call("read_process", json!({"job_id":job["job_id"]}))
            .await;
        if let Some(pid) = value["output"]
            .as_str()
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            break process(pid);
        }
        assert!(
            Instant::now() < deadline,
            "Service child never reported its identity: {value}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        process_sid(&child),
        sid,
        "Device jobs must retain the service identity"
    );
    let instance = peer.instance.clone();
    fixture.stop();
    let stopped = fixture.state(SERVICE_STOPPED).await;
    assert_eq!(stopped.dwWin32ExitCode, 0);
    assert_eq!(
        unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
        WAIT_OBJECT_0,
        "Service reported stopped before its child exited"
    );
    assert_eq!(
        unsafe { WaitForSingleObject(owner.as_raw_handle(), 5000) },
        WAIT_OBJECT_0
    );
    assert!(
        !config
            .with_file_name(format!(
                "{}.node-lock",
                config.file_name().unwrap().to_string_lossy()
            ))
            .exists()
    );
    assert_eq!(fs::read(&config).unwrap(), contents);
    drop(peer);
    fixture.start();
    let mut peer = Peer::connect(listener).await;
    assert_ne!(peer.instance, instance);
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"native.txt"}))
            .await["content"],
        "service 😀"
    );
    fixture.stop();
    fixture.state(SERVICE_STOPPED).await;
    drop(peer);
    fs::rename(&config, config.with_extension("saved")).unwrap();
    fixture.start();
    let failed = fixture.state(SERVICE_STOPPED).await;
    assert_ne!(
        failed.dwWin32ExitCode, 0,
        "Missing configuration must fail the service"
    );
    assert!(!config.exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "Service attempted a new pairing or connection without its config"
    );
    assert_ne!(unsafe { DeleteService(fixture.service.0) }, 0);
    fixture.removed = true;
}
