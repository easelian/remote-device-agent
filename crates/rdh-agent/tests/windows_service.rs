//! A real, uniquely named LocalSystem service with a loopback authenticated Hub.
#![cfg(all(windows, feature = "experimental-windows-startup"))]

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
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT,
            SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        LookupPrivilegeValueW, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
        TOKEN_QUERY,
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
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    },
};

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
            unsafe { LookupPrivilegeValueW(null(), SE_RESTORE_NAME, &mut next.Privileges[0].Luid) },
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

fn system_acl(path: &Path, directory: bool) {
    // Only fixture-owned paths are transferred to the account that will run
    // the service. No production profile or service account is changed.
    let inherited = if directory { "OICI" } else { "" };
    let sddl = wide(format!("O:SYD:P(A;{inherited};FA;;;SY)(A;{inherited};FA;;;BA)").as_ref());
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
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct Peer {
    socket: WebSocketStream<TcpStream>,
    instance: Value,
    next: u64,
}
impl Peer {
    async fn connect(listener: &TcpListener) -> Self {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(45), listener.accept())
            .await
            .expect("Service did not connect to the fixture Hub")
            .unwrap();
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
        tokio::time::timeout(Duration::from_secs(30), async {
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
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(
        !handle.is_null(),
        "Open fixture process: {}",
        io::Error::last_os_error()
    );
    unsafe { OwnedHandle::from_raw_handle(handle) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "creates and removes one native LocalSystem service on an elevated Windows runner"]
async fn native_windows_service_connects_drains_jobs_restarts_and_refuses_missing_config() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let name = format!("RDH-Test-Native-{}", random::hex_token::<12>().unwrap());
    let directory =
        PathBuf::from(std::env::var_os("ProgramData").unwrap()).join(format!("{name} '😀"));
    fs::create_dir(&directory).unwrap();
    let work = directory.join("work");
    fs::create_dir(&work).unwrap();
    let binary = directory.join("rdh.exe");
    fs::copy(env!("CARGO_BIN_EXE_rdh"), &binary).unwrap();
    let config = directory.join("device '😀.json");
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let contents = serde_json::to_vec(
        &json!({"gateway_url":origin,"device_id":"native-service","token":TOKEN,
        "roots":local_roots(&[format!("work={}",work.display())]).unwrap(),
        "allow_write":true,"allow_shell":true,"shell":powershell}),
    )
    .unwrap();
    fs::write(&config, &contents).unwrap();
    let policy = directory.join("device '😀.policy.json");
    fs::write(&policy, br#"{"version":1,"audit":{"enabled":false}}"#).unwrap();
    {
        let _privilege = Privilege::restore_owner();
        for path in [&binary, &config, &policy] {
            system_acl(path, false);
        }
        system_acl(&work, true);
        system_acl(&directory, true);
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
            null(),
            null(),
        )
    };
    assert!(
        !raw.is_null(),
        "Create fixture service: {}",
        io::Error::last_os_error()
    );
    let mut fixture = Fixture {
        directory,
        service: Handle(raw),
        removed: false,
    };
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let running = fixture.state(SERVICE_RUNNING).await;
    let owner = process(running.dwProcessId);
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
    let mut peer = Peer::connect(&listener).await;
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
