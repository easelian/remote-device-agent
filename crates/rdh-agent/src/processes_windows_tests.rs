//! Real Windows process/pipe/Job tests; no POSIX shell or external runtime needed.
use super::*;
use std::fs;
use std::process::{Command, Stdio};
use windows_sys::Win32::Foundation::GetHandleInformation;
use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, WAIT_OBJECT_0};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent,
};

const HELPER: &str = "processes::native::platform::tests::child_helper";

fn default_shell() -> String {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("cmd.exe")
        .to_str()
        .unwrap()
        .into()
}
fn powershell() -> String {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
        .to_str()
        .unwrap()
        .into()
}
fn config(path: &Path) -> DeviceConfig {
    DeviceConfig {
        gateway_url: "http://localhost".into(),
        device_id: "test".into(),
        token: "test".into(),
        roots: BTreeMap::from([("work".into(), path.to_str().unwrap().into())]),
        allow_shell: true,
        allow_write: true,
        shell: default_shell(),
        max_processes: 4,
        max_process_seconds: 20,
    }
}
struct Fixture {
    path: PathBuf,
    processes: Processes,
}
impl Fixture {
    fn new() -> Self {
        Self::configured(|_| {}, None)
    }
    fn configured(change: impl FnOnce(&mut DeviceConfig), hook: Option<ExitHook>) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rdh-process-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        let mut config = config(&path);
        change(&mut config);
        let files = Arc::new(Files::new(&config.roots, true).unwrap());
        Self {
            path,
            processes: Processes::new(&config, files, vec![], hook).unwrap(),
        }
    }
    fn call(&self, operation: &str, params: Value) -> Result<Value, ProcessFailure> {
        self.processes.dispatch(
            operation,
            &params,
            "original-request",
            Instant::now() + Duration::from_secs(20),
            &AtomicBool::new(false),
        )
    }
    fn start(&self, command: &str, wait: f64) -> Value {
        self.call(
            "start_process",
            json!({"root":"work","command":command,"wait_seconds":wait}),
        )
        .unwrap()
    }
    fn helper(&self, spec: Value) -> Value {
        fs::write(
            self.path.join("child-mode.json"),
            serde_json::to_vec(&spec).unwrap(),
        )
        .unwrap();
        self.start(&helper_command(), 0.0)
    }
    fn read(&self, id: &str) -> Value {
        self.call("read_process", json!({"job_id":id,"limit":65536}))
            .unwrap()
    }
    fn done(&self, id: &str) -> Value {
        until_for(
            Duration::from_secs(u64::from(self.processes.inner.config.max_process_seconds) + 5),
            || {
                let result = self.read(id);
                (result["status"] != "running" && result["cleanup_pending"] == false)
                    .then_some(result)
            },
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.processes.close(Duration::from_secs(5)) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
fn until<T>(mut check: impl FnMut() -> Option<T>) -> T {
    until_for(Duration::from_secs(15), &mut check)
}
fn until_for<T>(budget: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let expires = Instant::now() + budget;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(
            Instant::now() < expires,
            "Windows process fixture did not reach the required state"
        );
        thread::sleep(TICK);
    }
}
fn helper_command() -> String {
    format!(
        "\"{}\" --exact {HELPER} --nocapture",
        std::env::current_exe().unwrap().display()
    )
}
fn open_process(pid: u32) -> OwnedHandle {
    // SAFETY: test obtains a wait-only handle while the recorded child is live.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(
        !handle.is_null(),
        "open child {pid}: {}",
        io::Error::last_os_error()
    );
    // SAFETY: successful OpenProcess transfers one real handle.
    unsafe { OwnedHandle::from_raw_handle(handle) }
}
fn wait_process(process: &OwnedHandle) {
    // SAFETY: wait-only owned process identity, never a reused PID.
    assert_eq!(
        unsafe { WaitForSingleObject(process.as_raw_handle(), 10_000) },
        WAIT_OBJECT_0
    );
}
fn recorded_process(path: &Path) -> OwnedHandle {
    let pid = until(|| {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
    });
    open_process(pid)
}
fn mode(directory: &Path, value: Value) {
    fs::create_dir_all(directory).unwrap();
    fs::write(
        directory.join("child-mode.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

// This test also runs harmlessly in the normal test suite. Explicit child
// invocations select their behavior through a file in an isolated cwd.
#[test]
fn child_helper() {
    let path = std::env::current_dir().unwrap();
    let Ok(bytes) = fs::read(path.join("child-mode.json")) else {
        return;
    };
    let spec: Value = serde_json::from_slice(&bytes).unwrap();
    match spec["mode"].as_str().unwrap() {
        "echo" => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes).unwrap();
            io::stdout().write_all(&bytes).unwrap();
        }
        "overflow" => {
            let mut stdout = io::stdout().lock();
            for _ in 0..270 {
                stdout.write_all(&[b'x'; 1024]).unwrap();
            }
            // Force a multi-byte scalar across separate writes without a newline.
            stdout.write_all(&[0xf0, 0x9f]).unwrap();
            stdout.flush().unwrap();
            thread::sleep(Duration::from_millis(20));
            stdout.write_all(&[0x98, 0x80, b'Z']).unwrap();
        }
        "linger" => {
            fs::write(path.join("pid"), std::process::id().to_string()).unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        "tree" => {
            let nested = path.join("descendant");
            mode(&nested, json!({"mode":"linger"}));
            let silent = spec["silent"] == true;
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", HELPER, "--nocapture"])
                .current_dir(&nested)
                .stdin(Stdio::null());
            if silent {
                // Rust's Command inherits other inheritable handles as well
                // as the selected stdio. Null stdio alone would still let
                // this fixture's descendant retain the original pipe ends.
                // Clear those flags in this isolated helper before spawning
                // the deliberately silent descendant.
                for handle in [
                    io::stdin().as_raw_handle(),
                    io::stdout().as_raw_handle(),
                    io::stderr().as_raw_handle(),
                ] {
                    assert_ne!(
                        unsafe {
                            windows_sys::Win32::Foundation::SetHandleInformation(
                                handle,
                                HANDLE_FLAG_INHERIT,
                                0,
                            )
                        },
                        0
                    );
                }
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
            // This fixture deliberately outlives its parent. The Agent's Job
            // Object must terminate it, and the test observes its held handle.
            #[allow(clippy::zombie_processes)]
            let _child = command.spawn().unwrap();
            until(|| nested.join("pid").is_file().then_some(()));
            fs::write(path.join("tree-ready"), b"ready").unwrap();
            if spec["wait_release"] == true {
                until(|| path.join("release").is_file().then_some(()));
            }
            if spec["leader_exits"] != true {
                loop {
                    thread::sleep(Duration::from_secs(1));
                }
            }
        }
        "sentinel" => {
            let handle = spec["handle"].as_u64().unwrap() as usize as HANDLE;
            // SAFETY: Windows validates this deliberately untrusted child-local
            // handle value. Only our dedicated event could signal the parent.
            let signaled = unsafe { SetEvent(handle) } != 0;
            println!("sentinel-signaled={signaled}");
        }
        "crash-owner" => {
            let root = path.join("owned");
            mode(&root, json!({"mode":"tree", "silent":false}));
            let config = config(&root);
            let files = Arc::new(Files::new(&config.roots, true).unwrap());
            let manager = Processes::new(&config, files, vec![], None).unwrap();
            let result = manager
                .dispatch(
                    "start_process",
                    &json!({
                        "root":"work","command":helper_command(),"wait_seconds":0
                    }),
                    "crash-child",
                    Instant::now() + Duration::from_secs(15),
                    &AtomicBool::new(false),
                )
                .unwrap();
            until(|| root.join("tree-ready").is_file().then_some(()));
            fs::write(
                path.join("shell-pid"),
                result["pid"].as_u64().unwrap().to_string(),
            )
            .unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        other => panic!("Unknown process helper mode {other}"),
    }
    io::stdout().flush().unwrap();
    std::process::exit(0);
}

#[cfg(feature = "experimental-windows-install")]
#[test]
fn native_candidate_checks_use_an_owned_job_and_bound_output() {
    // The candidate checker passes native argv. cmd.exe has a distinct /C
    // command-string grammar, so use this native test executable as the probe.
    let native = std::env::current_exe().unwrap();
    let listed = format!("{HELPER}: test\n\n1 test, 0 benchmarks");
    check_program(
        &native,
        &["--list", "--exact", HELPER],
        &listed,
        false,
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(
        check_program(
            &native,
            &["--rdh-intentionally-unknown-option"],
            "",
            false,
            Duration::from_secs(10)
        )
        .is_err()
    );
    assert!(
        check_program(
            &native,
            &["--list", "--exact", HELPER],
            &listed,
            false,
            Duration::ZERO
        )
        .is_err()
    );

    let fixture = Fixture::new();
    let executable = fixture.path.join("candidate-helper.exe");
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    mode(
        &fixture.path,
        json!({"mode":"tree", "silent":true, "leader_exits":true, "wait_release":true}),
    );
    let worker = thread::spawn(move || {
        check_program(
            &executable,
            &["--exact", HELPER, "--nocapture"],
            "",
            false,
            Duration::from_secs(15),
        )
    });
    until(|| fixture.path.join("tree-ready").is_file().then_some(()));
    let descendant = recorded_process(&fixture.path.join("descendant/pid"));
    fs::write(fixture.path.join("release"), b"release").unwrap();
    assert!(worker.join().unwrap().is_err());
    wait_process(&descendant);

    mode(&fixture.path, json!({"mode":"overflow"}));
    assert!(
        check_program(
            &fixture.path.join("candidate-helper.exe"),
            &["--exact", HELPER, "--nocapture"],
            "",
            false,
            Duration::from_secs(10)
        )
        .unwrap_err()
        .to_string()
        .contains("too large")
    );
}

#[test]
fn cmd_shell_cwd_nonzero_exit_and_completion_hook() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let fixture = Fixture::configured(
        |_| {},
        Some(Arc::new(move |request, result| {
            lock(&captured).push((request, result));
        })),
    );
    fs::create_dir(fixture.path.join("nested")).unwrap();
    let launched = fixture.call(
        "start_process",
        json!({"root":"work","cwd":"nested",
        "command":"echo shell-marker&cd&echo error-marker 1>&2&exit /b 7","wait_seconds":2}),
    );
    if let Err(error) = &launched {
        // Preserve the original failure. These fixed, harmless builtin-only
        // commands distinguish cwd, command text and Job/handle launch paths.
        for (label, cwd, command) in [
            ("root-simple", ".", "echo diagnostic-marker"),
            ("nested-simple", "nested", "echo diagnostic-marker"),
            (
                "root-original",
                ".",
                "echo shell-marker&cd&echo error-marker 1>&2&exit /b 7",
            ),
        ] {
            let result = fixture.call(
                "start_process",
                json!({
                    "root":"work", "cwd":cwd, "command":command, "wait_seconds":2
                }),
            );
            match result {
                Ok(job) => {
                    let done = fixture.done(job["job_id"].as_str().unwrap());
                    eprintln!(
                        "Process fixture {label}: status={}, exit={}",
                        done["status"], done["exit_code"]
                    );
                }
                Err(error) => eprintln!("Process fixture {label}: {error:?}"),
            }
        }
        let direct = Command::new(default_shell())
            .args([
                "/d",
                "/s",
                "/c",
                "echo shell-marker&cd&echo error-marker 1>&2&exit /b 7",
            ])
            .current_dir(fixture.path.join("nested"))
            .env_clear()
            .envs(environment(&[]).into_values())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match direct {
            Ok(mut child) => {
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            eprintln!("Process fixture direct nested: {status}");
                            break;
                        }
                        Ok(None) if Instant::now() < deadline => thread::sleep(TICK),
                        result => {
                            let _ = child.kill();
                            let _ = child.wait();
                            eprintln!("Process fixture direct nested did not finish: {result:?}");
                            break;
                        }
                    }
                }
            }
            Err(error) => eprintln!("Process fixture direct nested spawn: {error}"),
        }
        panic!("Original cmd/cwd fixture launch failed: {error:?}");
    }
    let job = launched.unwrap();
    let result = fixture.done(job["job_id"].as_str().unwrap());
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exit_code"], 7);
    let output = result["output"].as_str().unwrap();
    assert!(output.contains("shell-marker") && output.contains("error-marker"));
    assert!(
        output.to_ascii_lowercase().contains(
            &fixture
                .path
                .join("nested")
                .display()
                .to_string()
                .to_ascii_lowercase()
        )
    );
    assert_eq!(lock(&calls).len(), 1);
    assert_eq!(lock(&calls)[0].0, "original-request");
    assert_eq!(lock(&calls)[0].1["cleanup_pending"], false);
}

#[test]
fn powershell_quotes_unicode_and_unsigned_exit_code_259() {
    // Prior native ARM64 runs observed roughly 25 s for Windows PowerShell
    // startup. This shell/quoting check does not test execution deadlines;
    // the timeout/tree test below retains its short Rust-helper deadline.
    let fixture = Fixture::configured(
        |config| {
            config.shell = powershell();
            config.max_process_seconds = 60;
        },
        None,
    );
    let job = fixture.start(r#"[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write('A😀B "quoted" C:\tail\'); exit 259"#, 2.0);
    let result = fixture.done(job["job_id"].as_str().unwrap());
    assert_eq!(result["status"], "exited");
    assert_eq!(result["exit_code"], 259);
    assert_eq!(result["output"], r#"A😀B "quoted" C:\tail\"#);
    let part = fixture
        .call(
            "read_process",
            json!({"job_id":job["job_id"], "offset":1, "limit":1}),
        )
        .unwrap();
    assert_eq!(part["output"], "😀");
    assert_eq!(part["next_offset"], 2);
    let high = fixture.start("exit -1", 2.0);
    assert_eq!(
        fixture.done(high["job_id"].as_str().unwrap())["exit_code"],
        4_294_967_295_u64
    );
}

#[test]
fn streaming_output_is_bounded_and_input_close_reaches_eof() {
    let fixture = Fixture::new();
    let echo = fixture.helper(json!({"mode":"echo"}));
    let input = fixture
        .call(
            "write_process_stdin",
            json!({"job_id":echo["job_id"],"text":"go😀\n","close":true}),
        )
        .unwrap();
    assert_eq!(input["sent"], 4);
    assert_eq!(input["stdin_closed"], true);
    let echoed = fixture.done(echo["job_id"].as_str().unwrap());
    assert!(echoed["output"].as_str().unwrap().ends_with("go😀\n"));
    let large = fixture.helper(json!({"mode":"overflow"}));
    let id = large["job_id"].as_str().unwrap();
    let result = fixture.done(id);
    assert_eq!(result["output_dropped"], true);
    let base = result["offset"].as_u64().unwrap();
    let tail = fixture
        .call(
            "read_process",
            json!({"job_id":id,"offset":base+MAX_OUTPUT as u64-2,"limit":2}),
        )
        .unwrap();
    assert_eq!(tail["output"], "😀Z");
    assert_eq!(tail["next_offset"], base + MAX_OUTPUT as u64);
}

#[test]
fn backpressured_stdin_times_out_and_termination_releases_the_writer() {
    let fixture = Fixture::new();
    let job = fixture.helper(json!({"mode":"linger"}));
    let pid = recorded_process(&fixture.path.join("pid"));
    let mut failed_write = None;
    for _ in 0..100 {
        let result = fixture.processes.dispatch(
            "write_process_stdin",
            &json!({
                "job_id":job["job_id"],"text":"x".repeat(16_384)
            }),
            "writer",
            Instant::now() + Duration::from_millis(200),
            &AtomicBool::new(false),
        );
        if let Err(error) = result {
            failed_write = Some(error);
            break;
        }
    }
    let error = failed_write.expect("child input must have bounded backpressure");
    assert_eq!(error.code, "operation_timeout");
    assert!(error.summary["bytes_written"].as_u64().unwrap() <= 16_384);
    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            fixture.call(
                "write_process_stdin",
                json!({"job_id":job["job_id"],"text":"more"}),
            )
        });
        thread::sleep(Duration::from_millis(30));
        fixture
            .call("terminate_process", json!({"job_id":job["job_id"]}))
            .unwrap();
        assert!(writer.join().unwrap().is_err());
    });
    let result = fixture.done(job["job_id"].as_str().unwrap());
    assert_eq!(result["status"], "terminated");
    wait_process(&pid);
}

#[test]
fn timeout_and_normal_leader_exit_both_remove_remaining_descendants() {
    for leader_exits in [false, true] {
        let fixture = Fixture::new();
        fs::write(fixture.path.join("child-mode.json"), serde_json::to_vec(&json!({
            "mode":"tree","silent":leader_exits,"leader_exits":leader_exits,"wait_release":leader_exits
        })).unwrap()).unwrap();
        let job = fixture
            .call(
                "start_process",
                json!({"root":"work","command":helper_command(),
            "timeout_seconds": if leader_exits {10} else {2}, "wait_seconds":0}),
            )
            .unwrap();
        // Pin the descendant before permitting normal leader exit. The
        // assertion never reopens a numeric PID after its lifetime has ended.
        let descendant = recorded_process(&fixture.path.join("descendant").join("pid"));
        if !leader_exits {
            let result = fixture.done(job["job_id"].as_str().unwrap());
            assert_eq!(result["status"], "timed_out");
            wait_process(&descendant);
        } else {
            fs::write(fixture.path.join("release"), b"release").unwrap();
            let result = fixture.done(job["job_id"].as_str().unwrap());
            assert_eq!(result["status"], "exited");
            assert_eq!(result["exit_code"], 0);
            wait_process(&descendant);
        }
    }
}

#[test]
fn cancellation_and_resume_failure_do_not_run_the_suspended_command() {
    for resume_failure in [false, true] {
        let fixture = Fixture::new();
        let plan = prepare(
            &fixture.processes.inner,
            "work",
            ".",
            "echo ran>should-not-exist",
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        let job = Arc::new(Job::new("cancelled-child".into(), "cancel-request"));
        let mut child = spawn(plan, &job).unwrap();
        let process = child.process.try_clone().unwrap();
        if resume_failure {
            // A real but non-thread handle causes ResumeThread to fail without
            // invalidating Rust's OwnedHandle invariant or adding production hooks.
            let event = unsafe { CreateEventW(null(), 1, 0, null()) };
            assert!(!event.is_null());
            child.thread = Some(unsafe { OwnedHandle::from_raw_handle(event) });
        } else {
            job.terminate();
        }
        supervise(&job, child, Duration::from_secs(5), None);
        wait_process(&process);
        let state = job.read(0.0, 10, false);
        assert_eq!(state["status"], "failed");
        assert_eq!(state["cleanup_pending"], false);
        assert!(!fixture.path.join("should-not-exist").exists());
    }
}

#[test]
fn spawn_failure_and_offline_recovery_preserve_admission() {
    let fixture = Fixture::configured(|config| config.max_processes = 1, None);
    fixture.processes.mark_offline();
    assert_eq!(
        fixture
            .call(
                "start_process",
                json!({"root":"work","command":"echo forbidden"})
            )
            .unwrap_err()
            .code,
        "operation_busy"
    );
    fixture.processes.mark_online();
    let started = fixture.helper(json!({"mode":"linger"}));
    let child = recorded_process(&fixture.path.join("pid"));
    assert_eq!(
        fixture
            .call(
                "start_process",
                json!({"root":"work","command":"echo full"})
            )
            .unwrap_err()
            .code,
        "operation_busy"
    );
    fixture.processes.mark_offline();
    fixture.processes.stop_offline_jobs();
    fixture.done(started["job_id"].as_str().unwrap());
    wait_process(&child);
    fixture.processes.mark_online();
    let next = fixture.start("exit /b 0", 1.0);
    assert_eq!(
        fixture.done(next["job_id"].as_str().unwrap())["status"],
        "exited"
    );
    assert!(fixture.processes.close(Duration::from_secs(3)));
    fixture.processes.mark_online();
    assert_eq!(
        fixture
            .call(
                "start_process",
                json!({"root":"work","command":"echo closed"})
            )
            .unwrap_err()
            .code,
        "operation_busy"
    );

    let failed = Fixture::configured(
        |config| config.shell = format!(r"{}\cmd.exe", config.roots["work"]),
        None,
    );
    fs::write(failed.path.join("cmd.exe"), b"not a PE image").unwrap();
    let error = failed
        .call(
            "start_process",
            json!({"root":"work","command":"echo fail"}),
        )
        .unwrap_err();
    assert_eq!(error.code, "process_start_failed");
    assert_eq!(error.state, "failed");
    assert!(failed.processes.close(Duration::from_secs(3)));
}

#[test]
fn unrelated_inheritable_handle_is_not_passed_to_the_child() {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let raw = unsafe { CreateEventW(&attributes, 1, 0, null()) };
    assert!(!raw.is_null());
    let event = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut flags = 0;
    assert_ne!(unsafe { GetHandleInformation(raw, &mut flags) }, 0);
    assert_ne!(flags & HANDLE_FLAG_INHERIT, 0);
    let fixture = Fixture::new();
    let job = fixture.helper(json!({"mode":"sentinel","handle":raw as usize as u64}));
    let result = fixture.done(job["job_id"].as_str().unwrap());
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("sentinel-signaled=false")
    );
    assert_eq!(
        unsafe { WaitForSingleObject(event.as_raw_handle(), 0) },
        WAIT_TIMEOUT
    );
}

#[test]
fn parent_crash_closes_the_job_and_kills_its_process_tree() {
    let fixture = Fixture::new();
    mode(&fixture.path, json!({"mode":"crash-owner"}));
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER, "--nocapture"])
        .current_dir(&fixture.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(|| {
        let shell = recorded_process(&fixture.path.join("shell-pid"));
        let descendant =
            recorded_process(&fixture.path.join("owned").join("descendant").join("pid"));
        (shell, descendant)
    });
    let _ = owner.kill();
    let _ = owner.wait();
    let (shell, descendant) = result.unwrap();
    wait_process(&shell);
    wait_process(&descendant);
}

#[test]
fn environment_is_case_insensitive_explicit_and_has_legacy_windows_basics() {
    let source = BTreeMap::from([
        ("PATH".into(), OsString::from(r"C:\custom")),
        ("SYSTEMROOT".into(), OsString::from(r"C:\Win")),
        ("PATHEXT".into(), OsString::from(".exe;.BAT")),
        ("RDH_SECRET".into(), OsString::from("hidden")),
        ("NODE_OPTIONS".into(), OsString::from("hidden")),
        ("HTTPS_PROXY".into(), OsString::from("hidden")),
        ("DISPLAY".into(), OsString::from("hidden")),
        ("HOMEDRIVE".into(), OsString::from("C:")),
        ("RDH_CUSTOM".into(), OsString::from("visible")),
    ]);
    let env = environment_from(&source, &["rdh_custom".into(), "RdH_CuStOm".into()]);
    assert_eq!(get_env(&env, "RDH_CUSTOM"), Some(OsStr::new("visible")));
    assert_eq!(get_env(&env, "PATH"), Some(OsStr::new(r"C:\custom")));
    assert_eq!(get_env(&env, "HOMEDRIVE"), Some(OsStr::new("C:")));
    assert_eq!(
        get_env(&env, "COMSPEC"),
        Some(OsStr::new(r"C:\Win\System32\cmd.exe"))
    );
    assert_eq!(
        get_env(&env, "PATHEXT"),
        Some(OsStr::new(".exe;.BAT;.COM;.CMD"))
    );
    for key in ["RDH_SECRET", "NODE_OPTIONS", "HTTPS_PROXY", "DISPLAY"] {
        assert!(!env.contains_key(key));
    }
    let block = environment_block(&env).unwrap();
    assert!(block.ends_with(&[0, 0]));
    let fallback = environment_from(&BTreeMap::new(), &[]);
    assert_eq!(
        get_env(&fallback, "PATH"),
        Some(OsStr::new(r"C:\Windows\System32"))
    );
}

#[test]
fn executable_search_uses_child_cwd_and_path_without_pathext() {
    let fixture = Fixture::new();
    let bin = fixture.path.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("cmd.exe"), b"fixture").unwrap();
    fs::write(fixture.path.join("cmd.bat"), b"fixture").unwrap();
    let env = environment_from(
        &BTreeMap::from([
            ("PATH".into(), "bin".into()),
            ("PATHEXT".into(), ".BAT".into()),
        ]),
        &[],
    );
    assert_eq!(
        executable("cmd", &fixture.path, &env).unwrap(),
        bin.join("cmd.exe")
    );
    fs::write(fixture.path.join("cmd.com"), b"fixture").unwrap();
    assert_eq!(
        executable("cmd", &fixture.path, &env).unwrap(),
        fixture.path.join("cmd.com")
    );
    assert_eq!(
        executable(r"bin\cmd.exe", &fixture.path, &env).unwrap(),
        bin.join("cmd.exe")
    );
    assert!(executable(r"missing\cmd.exe", &fixture.path, &env).is_err());
}

#[test]
fn repeated_short_processes_reuse_the_slot_after_cleanup() {
    let fixture = Fixture::configured(|config| config.max_processes = 1, None);
    for _ in 0..16 {
        let job = fixture.start("exit /b 0", 1.0);
        assert_eq!(
            fixture.done(job["job_id"].as_str().unwrap())["exit_code"],
            0
        );
    }
    assert!(fixture.processes.quiescent());
}
