//! Actual pinned shawl and old Node -> native SCM service -> old Node.
use super::*;
use std::{io::Read, os::windows::fs::OpenOptionsExt};
use windows_sys::Win32::{
    Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    },
    System::Services::{SERVICE_CHANGE_CONFIG, SERVICE_QUERY_CONFIG},
};

fn local(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(path.to_str().unwrap())
        .replace('/', "\\")
}
fn quote(value: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { 2 * slashes + 1 } else { slashes },
        ));
        result.push(ch);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', 2 * slashes));
    result.push('"');
    result
}
fn backup_file(path: &Path) -> fs::File {
    let privilege = Privilege::enable("SeBackupPrivilege");
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap();
    drop(privilege);
    file
}
fn file_state(path: &Path) -> ((u32, u32, u32), String, Vec<u8>) {
    let mut file = backup_file(path);
    let mut identity = BY_HANDLE_FILE_INFORMATION::default();
    assert_ne!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut identity) },
        0
    );
    let mut descriptor = null_mut();
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    assert_eq!(
        unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                information,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        },
        ERROR_SUCCESS
    );
    let mut text = null_mut();
    let mut length = 0;
    assert_ne!(
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                1,
                information,
                &mut text,
                &mut length,
            )
        },
        0
    );
    assert!(length > 0 && length < 65536);
    let security =
        String::from_utf16(unsafe { std::slice::from_raw_parts(text, length as usize - 1) })
            .unwrap();
    unsafe {
        LocalFree(text.cast());
        LocalFree(descriptor);
    }
    let mut bytes = Vec::new();
    if !file.metadata().unwrap().is_dir() {
        file.read_to_end(&mut bytes).unwrap();
    }
    (
        (
            identity.dwVolumeSerialNumber,
            identity.nFileIndexHigh,
            identity.nFileIndexLow,
        ),
        security,
        bytes,
    )
}
fn legacy_process(config: &Path, node: &Path) -> OwnedHandle {
    let owner: Value = serde_json::from_reader(backup_file(
        &PathBuf::from(format!("{}.node-lock", config.display())).join("owner.json"),
    ))
    .unwrap();
    let process = process(u32::try_from(owner["pid"].as_u64().unwrap()).unwrap());
    assert_eq!(process_image(&process), node.canonicalize().unwrap());
    assert_eq!(process_sid(&process), "S-1-5-19");
    process
}
async fn old_peer(listener: &TcpListener, fixture: &Fixture) -> Peer {
    // The old ARM64 program makes serial PowerShell ACL calls with unchanged
    // 60 s per-call limits. Bound the whole old startup/RPC composition here;
    // native fixture deadlines retain their existing values.
    Peer::connect_with_budget(listener, Duration::from_secs(900), Some(fixture)).await
}
async fn stop_legacy(fixture: &Fixture, owner: OwnedHandle) {
    fixture.stop();
    fixture.state(SERVICE_STOPPED).await;
    assert_eq!(
        unsafe { WaitForSingleObject(owner.as_raw_handle(), 20000) },
        WAIT_OBJECT_0
    );
}
fn fixture_lock(config: &Path, pid: u32) -> PathBuf {
    let lock = PathBuf::from(format!("{}.node-lock", config.display()));
    fs::create_dir(&lock).unwrap();
    let owner = lock.join("owner.json");
    fs::write(
        &owner,
        serde_json::to_vec(&json!({"pid":pid,"nonce":"a".repeat(32)})).unwrap(),
    )
    .unwrap();
    let _restore = Privilege::restore_owner();
    // Match New-PrivateAcl in the old Agent: even administrators have no
    // ordinary read access to this lock, and migration must preserve it.
    set_acl(&owner, &wide("O:LSD:P(A;;FA;;;LS)".as_ref()));
    set_acl(&lock, &wide("O:LSD:P(A;OICI;FA;;;LS)".as_ref()));
    lock
}
fn remove_fixture_lock(lock: &Path) {
    match fs::symlink_metadata(lock) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        result => assert!(result.unwrap().is_dir()),
    }
    // The fake owner or real fixture process has already stopped. Only this
    // test's generated lock receives an ACL change for fixture replacement.
    let restore = Privilege::restore_owner();
    service_acl(lock, true, "S-1-5-19");
    service_acl(&lock.join("owner.json"), false, "S-1-5-19");
    drop(restore);
    fs::remove_file(lock.join("owner.json")).unwrap();
    fs::remove_dir(lock).unwrap();
}
async fn audit_probe(peer: &mut Peer, data: &Path, cleanup: bool) {
    let state = data.join("remote-device-hub");
    let directory = state.join("audit");
    let audit = directory.join("native-service.jsonl");
    let literal = |p: &Path| format!("'{}'", local(p).replace('\'', "''"));
    let mut command = format!(
        "$ErrorActionPreference='Stop'; $s=[IO.File]::Open({},[IO.FileMode]::Open,[IO.FileAccess]::Read,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)); $r=[IO.StreamReader]::new($s); try {{$text=$r.ReadToEnd()}} finally {{$r.Dispose();$s.Dispose()}}; if (!$text.Contains('\"op\":\"write_file\"') -or $text.Contains('{TOKEN}')) {{exit 21}};",
        literal(&audit)
    );
    if cleanup {
        command.push_str(&format!("$admin=[Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'); foreach($path in @({},{},{})) {{$dir=[IO.Directory]::Exists($path);$sections=[Security.AccessControl.AccessControlSections]::Access;$acl=if($dir){{[IO.Directory]::GetAccessControl($path,$sections)}}else{{[IO.File]::GetAccessControl($path,$sections)}};$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($admin,'FullControl','Allow'));if($dir){{[IO.Directory]::SetAccessControl($path,$acl)}}else{{[IO.File]::SetAccessControl($path,$acl)}}}};", literal(&state), literal(&directory), literal(&audit)));
    }
    command.push_str("[Console]::Write('migration-audit-ok')");
    let started = peer
        .call(
            "start_process",
            json!({"root":"work","command":command,"timeout_seconds":60,"wait_seconds":0}),
        )
        .await;
    let deadline = Instant::now() + peer.receive_budget + Duration::from_secs(60);
    let result = loop {
        let result = peer
            .call("read_process", json!({"job_id":started["job_id"]}))
            .await;
        if result["status"] != "running" && result["cleanup_pending"] == false {
            break result;
        }
        assert!(
            Instant::now() < deadline,
            "Audit probe did not finish: {result}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["output"], "migration-audit-ok", "{result}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "uses actual Node and pinned shawl in one disposable elevated LocalService installation"]
async fn legacy_windows_service_handoff_preserves_profile_and_undo() {
    let _inspection = Privilege::inspect_service_processes();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let name = format!("RDH-Test-Legacy-{}", random::hex_token::<12>().unwrap());
    let directory =
        PathBuf::from(std::env::var_os("ProgramData").unwrap()).join(format!("{name} '😀"));
    fs::create_dir(&directory).unwrap();
    {
        let _restore = Privilege::restore_owner();
        program_acl(&directory, true);
    }
    let source = PathBuf::from(
        std::env::var_os("RDH_LEGACY_SOURCE").expect("actual old source/build required"),
    );
    let node_source =
        PathBuf::from(std::env::var_os("RDH_LEGACY_NODE").expect("actual native Node required"));
    let shawl_source = PathBuf::from(
        std::env::var_os("RDH_LEGACY_SHAWL").expect("build the pinned old shawl host"),
    );
    let package: Value =
        serde_json::from_slice(&fs::read(source.join("package.json")).unwrap()).unwrap();
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "arm64"
    };
    let legacy = directory.join("legacy");
    let release = legacy.join("versions").join(format!(
        "rdh-{}-win32-{arch}",
        package["version"].as_str().unwrap()
    ));
    fs::create_dir_all(release.join("bin")).unwrap();
    fs::create_dir_all(release.join("bundle")).unwrap();
    for relative in [
        "bin/remote-device-hub.mjs",
        "bundle/agent.mjs",
        "package.json",
    ] {
        fs::copy(source.join(relative), release.join(relative)).unwrap();
    }
    let node = release.join("node.exe");
    fs::copy(node_source, &node).unwrap();
    fs::copy(shawl_source, release.join("shawl.exe")).unwrap();
    let data = directory.join("data");
    let work = directory.join("work");
    fs::create_dir(&data).unwrap();
    fs::create_dir(&work).unwrap();
    let config = data.join("agent.json");
    let policy = data.join("agent.policy.json");
    let startup = data.join("agent.json.startup.json");
    let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let contents = serde_json::to_vec(&json!({"gateway_url":origin,"device_id":"native-service","token":TOKEN,
        "roots":local_roots(&[format!("work={}",work.display())]).unwrap(),"allow_write":true,"allow_shell":true,"shell":shell})).unwrap();
    fs::write(&config, &contents).unwrap();
    fs::write(
        &policy,
        br#"{"version":1,"audit":{"enabled":true,"required":true}}"#,
    )
    .unwrap();
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 24 * 60 * 60 * 1000;
    fs::write(&startup, serde_json::to_vec(&json!({"version":1,"binding":{"mode":"service","name":name},"pending":{"request_id":"01234567-89ab-4def-8123-456789abcdef","mode":"service","expires_at":expires}})).unwrap()).unwrap();
    {
        let _restore = Privilege::restore_owner();
        for file in [&config, &policy, &startup] {
            service_acl(file, false, "S-1-5-19");
        }
        service_acl(&data, true, "S-1-5-19");
        service_acl(&work, true, "S-1-5-19");
    }
    let original_files = [&config, &policy, &startup].map(|p| file_state(p));
    let command = [
        local(&release.join("shawl.exe")),
        "run".into(),
        "--name".into(),
        name.clone(),
        "--cwd".into(),
        local(&data),
        "--log-dir".into(),
        local(&data),
        "--env".into(),
        format!("XDG_STATE_HOME={}", local(&data)),
        "--stop-timeout".into(),
        "15000".into(),
        "--kill-process-tree".into(),
        "--restart".into(),
        "--restart-delay".into(),
        "5000".into(),
        "--".into(),
        local(&node),
        local(&release.join("bin/remote-device-hub.mjs")),
        "--config".into(),
        local(&config),
        "--server".into(),
        origin.clone(),
        "--no-browser".into(),
        "--recover-lock".into(),
    ]
    .iter()
    .map(|s| quote(s))
    .collect::<Vec<_>>()
    .join(" ");
    let manager = unsafe {
        OpenSCManagerW(
            null(),
            null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        )
    };
    assert!(!manager.is_null());
    let manager = Handle(manager);
    let service_name = wide(name.as_ref());
    let display = wide(format!("Remote Device Hub ({name})").as_ref());
    let account = wide(r"NT AUTHORITY\LocalService".as_ref());
    let cmd = wide(command.as_ref());
    let service = unsafe {
        CreateServiceW(
            manager.0,
            service_name.as_ptr(),
            display.as_ptr(),
            SERVICE_QUERY_STATUS
                | SERVICE_QUERY_CONFIG
                | SERVICE_CHANGE_CONFIG
                | SERVICE_START
                | SERVICE_STOP
                | 0x0001_0000,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            cmd.as_ptr(),
            null(),
            null_mut(),
            null(),
            account.as_ptr(),
            [0u16].as_ptr(),
        )
    };
    assert!(
        !service.is_null(),
        "Create legacy service: {}",
        io::Error::last_os_error()
    );
    let fixture = Fixture {
        directory,
        service: Handle(service),
        removed: false,
    };
    let before = service_configuration(fixture.service.0);
    let prefix = fixture.directory.join("native");
    let initial = initial_binary::path();
    let args = [
        "install-service",
        "--prefix",
        prefix.to_str().unwrap(),
        "--data",
        data.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--name",
        &name,
        "--migrate-from",
        legacy.to_str().unwrap(),
    ];
    let recover = [
        "install-service",
        "--recover-migration",
        "--prefix",
        prefix.to_str().unwrap(),
    ];
    let undo = [
        "install-service",
        "--undo-migration",
        "--prefix",
        prefix.to_str().unwrap(),
    ];
    // A stopped SCM registration alone does not prove the profile is idle.
    let live_lock = fixture_lock(&config, std::process::id());
    let live_owner = live_lock.join("owner.json");
    assert_eq!(
        fs::read(&live_owner).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let live_state = file_state(&live_owner);
    assert!(!service_manager(initial, &args).await.status.success());
    assert!(!prefix.exists());
    assert_eq!(file_state(&live_owner), live_state);
    remove_fixture_lock(&live_lock);
    fixture.start();
    let mut peer = old_peer(&listener, &fixture).await;
    let owner = legacy_process(&config, &node);
    peer.call(
        "write_file",
        json!({"root":"work","path":"handoff.txt","content":"original LocalService identity"}),
    )
    .await;
    assert!(!service_manager(initial, &args).await.status.success());
    assert!(!prefix.exists());
    stop_legacy(&fixture, owner).await;
    drop(peer);
    // Keep a terminated process handle so the synthetic stale owner's PID
    // cannot be recycled during the native handoff and recovery checks.
    let mut exited = std::process::Command::new(initial)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(exited.wait().unwrap().success());
    remove_fixture_lock(&live_lock);
    let stale_lock = fixture_lock(&config, exited.id());
    let stale_owner = stale_lock.join("owner.json");
    let stale_state = file_state(&stale_owner);
    let stale_directory_state = file_state(&stale_lock);
    manager_success(service_manager(initial, &args).await);
    manager_success(service_manager(initial, &recover).await);
    assert_eq!(file_state(&stale_owner), stale_state);
    assert_eq!(file_state(&stale_lock), stale_directory_state);
    assert_eq!(
        fs::read(&stale_owner).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let installed = prefix
        .join("versions")
        .join(env!("CARGO_PKG_VERSION"))
        .join("rdh.exe");
    let after = service_configuration(fixture.service.0);
    let mut preserved = after.clone();
    preserved["binary_path"] = before["binary_path"].clone();
    assert_eq!(preserved, before);
    for (path, state) in [&config, &policy, &startup]
        .into_iter()
        .zip(&original_files)
    {
        assert_eq!(&file_state(path), state);
    }
    // External program and SCM edits are refused before any registration write.
    let bundle = release.join("bundle/agent.mjs");
    let content = fs::read(&bundle).unwrap();
    fs::write(&bundle, b"foreign program").unwrap();
    assert!(!service_manager(initial, &undo).await.status.success());
    assert_eq!(service_configuration(fixture.service.0), after);
    fs::write(&bundle, &content).unwrap();
    set_service_command(
        fixture.service.0,
        after["binary_path"].as_str().unwrap(),
        Some("Independently edited fixture"),
    );
    let edited = service_configuration(fixture.service.0);
    assert!(!service_manager(initial, &undo).await.status.success());
    assert_eq!(service_configuration(fixture.service.0), edited);
    set_service_command(
        fixture.service.0,
        after["binary_path"].as_str().unwrap(),
        Some(before["display_name"].as_str().unwrap()),
    );
    fixture.start();
    let mut peer = Peer::connect(&listener).await;
    let owner = process(fixture.state(SERVICE_RUNNING).await.dwProcessId);
    assert_eq!(process_image(&owner), installed.canonicalize().unwrap());
    assert_eq!(process_sid(&owner), "S-1-5-19");
    let recovered_owner: Value = serde_json::from_reader(backup_file(&stale_owner)).unwrap();
    assert_ne!(recovered_owner["pid"], exited.id());
    drop(exited);
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"handoff.txt"}))
            .await["content"],
        "original LocalService identity"
    );
    audit_probe(&mut peer, &data, true).await;
    assert!(!service_manager(initial, &undo).await.status.success());
    stop_and_wait(&fixture, owner).await;
    drop(peer);
    exercise_service_updates(&listener, &fixture, &prefix, &config, &contents).await;
    manager_success(service_manager(initial, &undo).await);
    manager_success(service_manager(initial, &undo).await);
    assert_eq!(service_configuration(fixture.service.0), before);
    let blocked = service_manager(
        &installed,
        &["--config", config.to_str().unwrap(), "--server", &origin],
    )
    .await;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("native startup is blocked"));
    fixture.start();
    let mut peer = old_peer(&listener, &fixture).await;
    let owner = legacy_process(&config, &node);
    assert_eq!(
        peer.call("read_file", json!({"root":"work","path":"handoff.txt"}))
            .await["content"],
        "original LocalService identity"
    );
    audit_probe(&mut peer, &data, true).await;
    stop_legacy(&fixture, owner).await;
    drop(peer);
    let intent = prefix.join(".service-migration-intent");
    let undone = prefix.join(".service-migration-undone");
    // Reconstruct both sides of the single SCM publication, and the undo
    // boundary after its permanent startup barrier but before intent removal.
    for phase in 0..5 {
        if undone.exists() {
            fs::remove_file(&undone).unwrap();
        }
        let undoing = phase >= 2;
        fixture_program_record(&intent, &json!(if undoing { "undo" } else { "apply" }));
        let old_registration = phase == 0 || phase >= 3;
        set_service_command(
            fixture.service.0,
            if old_registration {
                &command
            } else {
                after["binary_path"].as_str().unwrap()
            },
            None,
        );
        if phase == 4 {
            fixture_program_record(&undone, &json!("undo"));
        }
        manager_success(service_manager(initial, &recover).await);
        assert!(!intent.exists());
        assert_eq!(undone.exists(), undoing);
        assert_eq!(
            service_configuration(fixture.service.0),
            if undoing {
                before.clone()
            } else {
                after.clone()
            }
        );
    }
    for (path, state) in [&config, &policy, &startup]
        .into_iter()
        .zip(&original_files)
    {
        assert_eq!(&file_state(path), state);
    }
    assert_eq!(fs::read(&bundle).unwrap(), content);
    assert!(node.is_file() && release.join("shawl.exe").is_file() && installed.is_file());
}
