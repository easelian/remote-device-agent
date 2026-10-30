//! POSIX spawn and cleanup with a pinned, unreaped process-group leader.
#[cfg(target_os = "macos")]
use super::super::macos::group_has_live_members;
use super::*;
#[cfg(any(target_os = "linux", test))]
use std::fs;
pub(super) use std::process::Child;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::{
        fd::AsRawFd,
        unix::fs::MetadataExt,
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{ChildStdin, Command, Stdio},
};
pub(super) type Input = ChildStdin;

pub(super) const TREE_TERMINATION: &str = "process-group";

#[cfg(target_os = "linux")]
const GROUP_CHECK: &str = "procfs_cleanup_check";
#[cfg(target_os = "macos")]
const GROUP_CHECK: &str = "process_group_cleanup_check";

pub(super) fn cancelled_error() -> io::Error {
    io::Error::from_raw_os_error(libc::ECANCELED)
}

pub(super) struct Plan {
    directory: std::fs::File,
    executable: std::path::PathBuf,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
}

pub(super) fn prepare(
    inner: &Inner,
    root: &str,
    cwd: &str,
    command: &str,
    _: Instant,
) -> Result<Plan, ProcessFailure> {
    let directory = inner.files.working_directory(root, cwd).map_err(invalid)?;
    let workdir = std::path::Path::new(
        inner
            .config
            .roots
            .get(root)
            .ok_or_else(|| invalid("Unknown root"))?,
    )
    .join(cwd);
    if inner.sandbox.is_isolated() {
        let named = std::fs::metadata(&workdir).map_err(|error| invalid(error.to_string()))?;
        let held = directory
            .metadata()
            .map_err(|error| invalid(error.to_string()))?;
        if named.dev() != held.dev() || named.ino() != held.ino() {
            return Err(invalid("Working directory changed while preparing sandbox"));
        }
    }
    let (executable, arguments) = inner
        .sandbox
        .command(&workdir, command)
        .map_err(|error| invalid(error.to_string()))?;
    let environment = environment(&inner.inherit_env, !inner.sandbox.is_isolated());
    Ok(Plan {
        directory,
        executable,
        arguments,
        environment,
    })
}

pub(super) fn spawn(plan: Plan, job: &Arc<Job>) -> io::Result<Child> {
    let mut command = Command::new(plan.executable);
    command
        .args(plan.arguments)
        .env_clear()
        .envs(plan.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let directory_fd = plan.directory.as_raw_fd();
    let watched = Arc::clone(job);
    // SAFETY: only async-signal-safe calls and atomic loads run after fork.
    // plan retains the directory until spawn returns; no pathname is reopened.
    unsafe {
        command.pre_exec(move || {
            if watched.stop.load(Ordering::Acquire) {
                return Err(cancelled_error());
            }
            if libc::setsid() == -1 || libc::fchdir(directory_fd) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

pub(super) fn os_error(error: &io::Error, syscall: &str) -> Value {
    let errno = error.raw_os_error();
    let code = match errno {
        Some(libc::ENOENT) => "ENOENT",
        Some(libc::EACCES) => "EACCES",
        Some(libc::ENOEXEC) => "ENOEXEC",
        Some(libc::EAGAIN) => "EAGAIN",
        Some(libc::ENOMEM) => "ENOMEM",
        Some(libc::EPIPE) => "EPIPE",
        Some(libc::ECHILD) => "ECHILD",
        Some(libc::EPERM) => "EPERM",
        Some(libc::EMFILE) => "EMFILE",
        Some(libc::ENFILE) => "ENFILE",
        _ => "OS_ERROR",
    };
    json!({"name":"Error", "code":code, "errno":errno, "syscall":syscall})
}
fn environment(inherit: &[String], session: bool) -> BTreeMap<OsString, OsString> {
    let mut env = BTreeMap::new();
    let common = [
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TERM",
    ];
    let session_names = [
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "XDG_DATA_DIRS",
        "XDG_CONFIG_DIRS",
    ];
    for name in common
        .into_iter()
        .chain(session_names.into_iter().filter(|_| session))
        .chain(inherit.iter().map(String::as_str))
    {
        if let Some(value) = std::env::var_os(name) {
            env.insert(name.into(), value);
        }
    }
    if env
        .get(std::ffi::OsStr::new("PATH"))
        .is_none_or(|value| value.is_empty())
    {
        env.insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into());
    }
    env
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn observe(pid: u32) -> io::Result<Option<(Option<i32>, Option<i32>)>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        (Some(status), None)
    } else {
        (None, Some(status))
    }))
}
fn signal_group(pid: u32) -> io::Result<()> {
    // ECHILD means the identity pin is lost. Never signal an old numeric PID
    // after that point, even if an OS snapshot contains a matching number.
    observe(pid)?;
    if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        // Darwin excludes zombies from killpg's signalable members and
        // returns EPERM when none remain. Accept that result only when a
        // complete snapshot proves the pinned zombie is the sole member.
        // An unreadable snapshot or any other member still preserves EPERM.
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EPERM)
            && matches!(group_has_live_members(pid), Ok(false))
        {
            return Ok(());
        }
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn vanished(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}
#[cfg(target_os = "linux")]
fn proc_state(path: &std::path::Path) -> io::Result<(u32, u8)> {
    let bytes = fs::read(path)?;
    let end = bytes
        .iter()
        .rposition(|b| *b == b')')
        .ok_or_else(|| io::Error::other("Invalid proc stat"))?;
    let fields = bytes[end + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let state = fields
        .first()
        .and_then(|value| value.first())
        .copied()
        .ok_or_else(|| io::Error::other("Invalid proc state"))?;
    let pgrp = fields
        .get(2)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("Invalid proc pgrp"))?;
    Ok((pgrp, state))
}
#[cfg(target_os = "linux")]
fn group_has_live_members(pid: u32) -> io::Result<bool> {
    // SIGKILL plus a complete procfs pass prevents an ordinary fork race
    // from being mistaken for completed cleanup. The zombie leader pins the
    // group throughout. A zombie thread-group leader can still have live
    // threads, so inspect its task directory as well.
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        match proc_state(&entry.path().join("stat")) {
            Ok((group, _)) if group != pid => continue,
            Ok((_, state)) if !matches!(state, b'Z' | b'X') => return Ok(true),
            Ok(_) => {}
            Err(error) if vanished(&error) => continue,
            Err(error) => return Err(error),
        }
        let tasks = match fs::read_dir(entry.path().join("task")) {
            Ok(tasks) => tasks,
            Err(error) if vanished(&error) => continue,
            Err(error) => return Err(error),
        };
        for task in tasks {
            match proc_state(&task?.path().join("stat")) {
                Ok((group, state)) if group == pid && !matches!(state, b'Z' | b'X') => {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(error) if vanished(&error) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(false)
}
pub(super) fn supervise(job: &Job, mut child: Child, timeout: Duration, hook: Option<&ExitHook>) {
    let pid = child.id();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdin = child.stdin.take();
    let prepared = [
        stdin.as_ref().map(AsRawFd::as_raw_fd),
        stdout.as_ref().map(AsRawFd::as_raw_fd),
        stderr.as_ref().map(AsRawFd::as_raw_fd),
    ]
    .into_iter()
    .try_for_each(|fd| {
        fd.ok_or_else(|| io::Error::other("Missing child pipe"))
            .and_then(nonblocking)
    });
    let mut output_failed = false;
    {
        let mut state = lock(&job.state);
        state.pid = Some(pid);
        if let Err(error) = prepared {
            state.status = "failed";
            state.error = Some(os_error(&error, "fcntl"));
            job.stop.store(true, Ordering::Release);
            output_failed = true;
        }
    }
    if output_failed {
        drop(stdin);
        stdout.take();
        stderr.take();
    } else {
        *lock(&job.stdin) = stdin;
    }
    lock(&job.state).ready = true;
    job.changed.notify_all();
    let expires = Instant::now() + timeout;
    let mut decoders = [Decoder::default(), Decoder::default()];
    let mut last_signal = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut stopping = output_failed;
    loop {
        let observed = match observe(pid) {
            Ok(observed) => observed,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // Without the leader pin cleanup cannot be proved. Keep its
                // admission slot forever rather than risk signalling reuse.
                let mut state = lock(&job.state);
                state.status = "failed";
                state.error = Some(os_error(&error, "waitid"));
                drop(state);
                lock(&job.stdin).take();
                job.changed.notify_all();
                return;
            }
        };
        if let Some((code, signal)) = observed {
            let mut state = lock(&job.state);
            state.exit_code = code.map(i64::from);
            state.signal = signal;
        }
        for result in [
            drain(&mut stdout, &mut decoders[0], job),
            drain(&mut stderr, &mut decoders[1], job),
        ] {
            if let Err(error) = result {
                let mut state = lock(&job.state);
                state.error = Some(os_error(&error, "read"));
                state.status = "failed";
                stopping = true;
                stdout.take();
                stderr.take();
            }
        }
        if job.stop.load(Ordering::Acquire) {
            stopping = true;
        }
        if !stopping && observed.is_some() && stdout.is_none() && stderr.is_none() {
            let mut state = lock(&job.state);
            if state.status == "running" {
                state.status = "exited";
            }
            stopping = true;
        }
        if !stopping && Instant::now() >= expires {
            lock(&job.state).status = "timed_out";
            stopping = true;
        }
        if stopping {
            lock(&job.stdin).take();
            if last_signal.elapsed() >= Duration::from_millis(20) {
                if let Err(error) = signal_group(pid) {
                    lock(&job.state).error = Some(os_error(&error, "killpg"));
                    // A later pass can retry EPERM/etc., but never after ECHILD.
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        return;
                    }
                }
                last_signal = Instant::now();
            }
            if observed.is_some() && stdout.is_none() && stderr.is_none() {
                match group_has_live_members(pid) {
                    Ok(false) => {
                        // All live members are gone while the leader remains
                        // unreaped. No group signal is ever sent after wait.
                        match child.wait() {
                            Ok(status) => {
                                let mut state = lock(&job.state);
                                state.exit_code = status.code().map(i64::from);
                                state.signal = status.signal();
                                if state.status == "running" {
                                    state.status = "exited";
                                }
                            }
                            Err(error) => {
                                lock(&job.state).error = Some(os_error(&error, "waitpid"));
                                return;
                            }
                        }
                        drop(child);
                        finish(job, hook);
                        return;
                    }
                    Ok(true) => {}
                    Err(error) => {
                        lock(&job.state).error = Some(os_error(&error, GROUP_CHECK));
                    }
                }
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: stdout.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 5);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    struct Fixture {
        path: PathBuf,
        processes: Processes,
    }
    impl Fixture {
        fn new() -> Self {
            Self::configured(|_| {}, None, vec![])
        }
        fn configured(
            change: impl FnOnce(&mut DeviceConfig),
            hook: Option<ExitHook>,
            inherit: Vec<String>,
        ) -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh-rust-process-{}-{}",
                std::process::id(),
                crate::random::hex_token::<12>().unwrap()
            ));
            fs::create_dir(&path).unwrap();
            let mut config = DeviceConfig {
                gateway_url: "http://localhost".into(),
                device_id: "test".into(),
                token: "test".into(),
                roots: BTreeMap::from([("work".into(), path.to_str().unwrap().into())]),
                allow_shell: true,
                allow_write: true,
                shell: "/bin/sh".into(),
                max_processes: 4,
                max_process_seconds: 10,
            };
            change(&mut config);
            let files = Arc::new(Files::new(&config.roots, config.allow_write).unwrap());
            let processes = Processes::new(&config, files, inherit, hook).unwrap();
            Self { path, processes }
        }
        fn call(&self, operation: &str, params: Value) -> Result<Value, ProcessFailure> {
            self.processes.dispatch(
                operation,
                &params,
                "original-request",
                Instant::now() + Duration::from_secs(15),
                &AtomicBool::new(false),
            )
        }
        fn start(&self, command: &str, wait: f64) -> Value {
            self.call(
                "start_process",
                json!({"root":"work", "command":command, "wait_seconds":wait}),
            )
            .unwrap()
        }
        fn read(&self, id: &str) -> Value {
            self.call("read_process", json!({"job_id":id})).unwrap()
        }
        fn done(&self, id: &str) -> Value {
            let expires = Instant::now() + Duration::from_secs(8);
            loop {
                let result = self.read(id);
                if result["status"] != "running" && result["cleanup_pending"] == false {
                    return result;
                }
                assert!(Instant::now() < expires, "process did not finish: {result}");
                thread::sleep(TICK);
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.processes.close(Duration::from_secs(5)) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }

    #[test]
    fn short_process_shell_syntax_unicode_cwd_and_exit_hook() {
        let exits = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&exits);
        let hook: ExitHook =
            Arc::new(move |request, result| lock(&captured).push((request, result)));
        let fixture = Fixture::configured(|_| {}, Some(hook), vec![]);
        fs::create_dir(fixture.path.join("nested")).unwrap();
        let job = fixture.call("start_process", json!({
            "root":"work", "cwd":"nested", "command":"printf 'diagnostic' >&2; printf 'A😀B'; pwd; exit 7", "wait_seconds":2,
        })).unwrap();
        let id = job["job_id"].as_str().unwrap();
        let final_result = fixture.done(id);
        assert_eq!(final_result["status"], "exited");
        assert_eq!(final_result["exit_code"], 7);
        assert!(
            final_result["output"]
                .as_str()
                .unwrap()
                .starts_with("diagnosticA😀B")
        );
        assert!(
            final_result["output"]
                .as_str()
                .unwrap()
                .contains(fixture.path.join("nested").to_str().unwrap())
        );
        let part = fixture
            .call("read_process", json!({"job_id":id,"offset":11,"limit":1}))
            .unwrap();
        assert_eq!(part["output"], "😀");
        assert_eq!(part["next_offset"], 12);
        assert_eq!(lock(&exits).len(), 1);
        assert_eq!(lock(&exits)[0].0, "original-request");
        assert_eq!(lock(&exits)[0].1["cleanup_pending"], false);
        assert!(
            fixture
                .call(
                    "start_process",
                    json!({"root":"work","cwd":"../","command":"true"})
                )
                .is_err()
        );
    }

    #[test]
    fn process_cursors_accept_large_integral_numbers_without_reading_output() {
        let fixture = Fixture::new();
        let job = fixture.start("printf 'A😀B'", 2.0);
        let id = job["job_id"].as_str().unwrap();
        fixture.done(id);
        for offset in [9_007_199_254_740_992.0, 1e100, f64::MAX] {
            let result = fixture
                .call(
                    "read_process",
                    json!({"job_id":id,"offset":offset,"limit":1}),
                )
                .unwrap();
            assert_eq!(result["output"], "");
            assert_eq!(result["offset"].as_f64(), Some(offset));
            assert_eq!(result["next_offset"].as_f64(), Some(offset));
            assert_eq!(result["output_dropped"], false);
        }
        for params in [
            json!({"job_id":id,"offset":-1}),
            json!({"job_id":id,"offset":0.5}),
            json!({"job_id":id,"offset":null}),
            json!({"job_id":id,"limit":0}),
            json!({"job_id":id,"limit":null}),
        ] {
            let error = fixture.call("read_process", params).unwrap_err();
            assert_eq!(error.message, "Invalid offset/limit");
            assert_eq!(error.code, "invalid_params");
            assert_eq!(error.state, "not_sent");
        }
    }

    #[test]
    fn failed_spawn_retains_sanitized_job() {
        let fixture = Fixture::configured(
            |config| config.shell = "/definitely-missing-rdh-shell".into(),
            None,
            vec![],
        );
        let error = fixture
            .call(
                "start_process",
                json!({"root":"work","command":"private command and credential","wait_seconds":0}),
            )
            .unwrap_err();
        assert_eq!(error.code, "process_start_failed");
        assert_eq!(error.state, "failed");
        assert!(!error.message.contains("private"));
        assert!(!error.summary.to_string().contains("credential"));
        let result = fixture.done(error.summary["job_id"].as_str().unwrap());
        assert_eq!(result["status"], "failed");
        assert_eq!(result["error"]["code"], "ENOENT");
        assert_eq!(
            fixture.call("list_processes", json!({})).unwrap()["jobs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn utf8_decoder_and_large_output_are_bounded_in_code_points() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.decode(&[0xf0, 0x9f], false), "");
        assert_eq!(
            decoder.decode(&[0x98, 0x80, b'x', 0xff, 0xe2], false),
            "😀x�"
        );
        assert_eq!(decoder.decode(&[], true), "�");
        let fixture = Fixture::new();
        let job = fixture.start("i=0; while [ $i -lt 262150 ]; do printf '😀'; i=$((i+1)); done; printf '\\342'; sleep 0.02; printf '\\202\\254'", 5.0);
        let id = job["job_id"].as_str().unwrap();
        fixture.done(id);
        let result = fixture
            .call("read_process", json!({"job_id":id,"offset":0,"limit":1}))
            .unwrap();
        assert_eq!(result["offset"], 7);
        assert_eq!(result["output_dropped"], true);
        assert_eq!(result["output"], "😀");
        let last = fixture
            .call(
                "read_process",
                json!({"job_id":id,"offset":262150,"limit":1}),
            )
            .unwrap();
        assert_eq!(last["output"], "€");
        let internal = fixture.processes.job(&json!({"job_id":id})).unwrap();
        let state = lock(&internal.state);
        assert_eq!(state.output.chars.len(), MAX_OUTPUT);
        assert!(state.output.bytes <= MAX_OUTPUT * 4);
        assert!(state.output.chars.capacity() * std::mem::size_of::<char>() <= MAX_OUTPUT * 4);
    }

    #[test]
    fn stdin_unicode_close_and_simultaneous_large_output() {
        let fixture = Fixture::new();
        // Fill stdout before reading stdin. A supervisor that waits on input
        // instead of continuing to drain output deadlocks this command.
        let job = fixture.start(
            "dd if=/dev/zero bs=1048576 count=1 2>/dev/null | tr '\\000' x; cat",
            0.0,
        );
        let id = job["job_id"].as_str().unwrap();
        let text = "😀".repeat(4096);
        let written = fixture
            .call(
                "write_process_stdin",
                json!({"job_id":id,"text":text,"close":true}),
            )
            .unwrap();
        assert_eq!(written["sent"], 4096);
        assert_eq!(written["stdin_closed"], true);
        fixture.done(id);
        let tail = fixture
            .call(
                "read_process",
                json!({"job_id":id,"offset":1048576,"limit":4096}),
            )
            .unwrap();
        assert_eq!(tail["output"], text);
        assert!(
            fixture
                .call("write_process_stdin", json!({"job_id":id,"text":"late"}))
                .is_err()
        );
    }

    #[test]
    fn stdin_backpressure_is_bounded_and_reports_partial_outcome() {
        let fixture = Fixture::new();
        let job = fixture.start("sleep 10", 0.0);
        let id = job["job_id"].as_str().unwrap();
        let started = Instant::now();
        let mut failure = None;
        for _ in 0..128 {
            if let Err(error) = fixture.call(
                "write_process_stdin",
                json!({"job_id":id,"text":"x".repeat(16384)}),
            ) {
                failure = Some(error);
                break;
            }
        }
        let error = failure.expect("pipe never applied backpressure");
        assert_eq!(error.code, "operation_timeout");
        assert!(started.elapsed() >= Duration::from_millis(2800));
        assert!(started.elapsed() < Duration::from_secs(5));
        if error.summary["bytes_written"].as_u64().unwrap() > 0 {
            assert_eq!(error.state, "unknown");
            assert!(error.message.contains("outcome is unknown"));
        }
        fixture
            .call("terminate_process", json!({"job_id":id}))
            .unwrap();
    }

    #[test]
    fn optional_five_second_wait_does_not_serialize_other_starts() {
        let fixture = Fixture::new();
        thread::scope(|scope| {
            let waiting = scope.spawn(|| fixture.start("sleep 1", 5.0));
            thread::sleep(Duration::from_millis(40));
            let started = Instant::now();
            let second = fixture.start("true", 0.2);
            assert!(started.elapsed() < Duration::from_millis(700));
            assert_eq!(
                fixture.done(second["job_id"].as_str().unwrap())["status"],
                "exited"
            );
            assert_eq!(waiting.join().unwrap()["status"], "exited");
        });
    }

    #[test]
    fn default_timeout_respects_configuration_and_offline_admission() {
        let fixture = Fixture::configured(|config| config.max_process_seconds = 1, None, vec![]);
        let job = fixture.start("sleep 30", 2.0);
        assert_eq!(
            fixture.done(job["job_id"].as_str().unwrap())["status"],
            "timed_out"
        );
        fixture.processes.mark_offline();
        assert!(
            fixture
                .call("start_process", json!({"root":"work","command":"true"}))
                .is_err()
        );
        fixture.processes.mark_online();
        assert_eq!(fixture.start("true", 1.0)["status"], "exited");
        assert!(fixture.processes.close(Duration::from_secs(1)));
        fixture.processes.mark_online();
        assert!(
            fixture
                .call("start_process", json!({"root":"work","command":"true"}))
                .is_err()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn signaling_a_pinned_zombie_does_not_report_permission_failure() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        while observe(pid).unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixture child did not exit");
            }
            thread::sleep(TICK);
        }
        assert!(!group_has_live_members(pid).unwrap());
        let signaled = signal_group(pid);
        let status = child.wait().unwrap();
        assert!(signaled.is_ok(), "{signaled:?}");
        assert_eq!(status.code(), Some(7));
        assert_eq!(
            signal_group(pid).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn exited_parent_keeps_identity_until_descendant_pipes_close() {
        let fixture = Fixture::new();
        let job = fixture.start("sleep 30 & printf 'parent-finished'; exit 7", 0.1);
        let id = job["job_id"].as_str().unwrap();
        let pid = job["pid"].as_u64().unwrap() as u32;
        let deadline = Instant::now() + Duration::from_secs(3);
        while observe(pid).unwrap().is_none() {
            assert!(Instant::now() < deadline, "parent did not exit");
            thread::sleep(TICK);
        }
        assert_eq!(observe(pid).unwrap().unwrap().0, Some(7));
        assert!(group_has_live_members(pid).unwrap());
        #[cfg(target_os = "linux")]
        assert_eq!(
            proc_state(&PathBuf::from(format!("/proc/{pid}/stat")))
                .unwrap()
                .1,
            b'Z'
        );
        let ended = fixture
            .call("terminate_process", json!({"job_id":id}))
            .unwrap();
        assert_eq!(fixture.done(id)["status"], "terminated");
        assert_eq!(ended["exit_code"], 7);
        assert_eq!(observe(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
        assert_eq!(
            signal_group(pid).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
        // Repeated termination is a read of the completed job, never an old
        // numeric PGID signal. PID reuse itself cannot be forced portably.
        assert_eq!(
            fixture
                .call("terminate_process", json!({"job_id":id}))
                .unwrap()["cleanup_pending"],
            false
        );
    }

    #[test]
    fn fork_race_termination_and_close_leave_no_live_group_members() {
        let fixture = Fixture::new();
        let job = fixture.start("while :; do sleep 30 & sleep 0.003; done", 0.05);
        let id = job["job_id"].as_str().unwrap();
        let pid = job["pid"].as_u64().unwrap() as u32;
        fixture
            .call("terminate_process", json!({"job_id":id}))
            .unwrap();
        fixture.done(id);
        #[cfg(target_os = "linux")]
        assert!(!group_has_live_members(pid).unwrap());
        #[cfg(target_os = "macos")]
        assert!(group_has_live_members(pid).is_err()); // The leader has been reaped.
        let another = fixture.start("sleep 30", 0.0);
        assert!(fixture.processes.close(Duration::from_secs(3)));
        assert_eq!(
            fixture.read(another["job_id"].as_str().unwrap())["cleanup_pending"],
            false
        );
    }

    #[test]
    fn cleanup_hook_holds_capacity_and_close_cannot_claim_completion() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::clone(&release);
        let entered = Arc::clone(&calls);
        let hook: ExitHook = Arc::new(move |_, _| {
            entered.fetch_add(1, Ordering::Release);
            let mut ready = lock(&gate.0);
            while !*ready {
                ready = gate.1.wait(ready).unwrap();
            }
        });
        let fixture = Fixture::configured(|config| config.max_processes = 1, Some(hook), vec![]);
        let job = fixture.start("true", 0.0);
        let expires = Instant::now() + Duration::from_secs(3);
        while calls.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < expires);
            thread::sleep(TICK);
        }
        let id = job["job_id"].as_str().unwrap();
        assert_eq!(fixture.read(id)["cleanup_pending"], true);
        assert!(!fixture.processes.quiescent());
        assert_eq!(
            fixture
                .call("start_process", json!({"root":"work","command":"true"}))
                .unwrap_err()
                .code,
            "operation_busy"
        );
        assert!(!fixture.processes.close(Duration::from_millis(20)));
        *lock(&release.0) = true;
        release.1.notify_all();
        assert!(fixture.processes.close(Duration::from_secs(2)));
        assert_eq!(fixture.read(id)["cleanup_pending"], false);
        assert!(fixture.processes.quiescent());
    }

    #[test]
    fn deadline_before_effect_leaves_no_job_and_does_not_send_stdin() {
        let fixture = Fixture::new();
        let expired = Instant::now() - Duration::from_millis(1);
        let closing = AtomicBool::new(false);
        let error = fixture
            .processes
            .dispatch(
                "start_process",
                &json!({"root":"work","command":"touch forbidden"}),
                "expired",
                expired,
                &closing,
            )
            .unwrap_err();
        assert_eq!(error.state, "not_sent");
        assert!(lock(&fixture.processes.inner.registry).jobs.is_empty());
        assert!(!fixture.path.join("forbidden").exists());
        let job = fixture.start("cat", 0.0);
        let id = job["job_id"].as_str().unwrap();
        assert_eq!(
            fixture
                .processes
                .dispatch(
                    "write_process_stdin",
                    &json!({"job_id":id,"text":"forbidden"}),
                    "expired",
                    expired,
                    &closing
                )
                .unwrap_err()
                .state,
            "not_sent"
        );
        fixture
            .call(
                "write_process_stdin",
                json!({"job_id":id,"text":"allowed","close":true}),
            )
            .unwrap();
        assert_eq!(fixture.done(id)["output"], "allowed");
    }

    #[test]
    fn retained_history_is_capped_at_64() {
        let fixture = Fixture::new();
        let first = fixture.start("true", 1.0)["job_id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..64 {
            fixture.start("true", 1.0);
        }
        assert_eq!(
            fixture.call("list_processes", json!({})).unwrap()["jobs"]
                .as_array()
                .unwrap()
                .len(),
            64
        );
        assert!(
            fixture
                .call("read_process", json!({"job_id":first}))
                .is_err()
        );
    }

    #[test]
    fn environment_isolation_real_child() {
        // Put test variables on a separate test-binary process; mutating this
        // multithreaded test runner's global environment would be unsound.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "processes::native::platform::tests::environment_helper",
                "--nocapture",
            ])
            .env("RDH_PROCESS_ENV_HELPER", "1")
            .env("HTTP_PROXY", "private-proxy")
            .env("NODE_OPTIONS", "private-node-option")
            .env("AWS_SECRET_ACCESS_KEY", "private-credential")
            .env("RDH_ALLOWED", "explicit-value")
            .env("DISPLAY", "session-value")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("environment-helper-completed"));
    }

    #[test]
    fn environment_helper() {
        if std::env::var("RDH_PROCESS_ENV_HELPER").as_deref() != Ok("1") {
            return;
        }
        let fixture = Fixture::configured(|_| {}, None, vec!["RDH_ALLOWED".into()]);
        let job = fixture.start("printf '%s|%s|%s|%s|%s' \"$HTTP_PROXY\" \"$NODE_OPTIONS\" \"$AWS_SECRET_ACCESS_KEY\" \"$RDH_ALLOWED\" \"$DISPLAY\"", 1.0);
        assert_eq!(
            fixture.done(job["job_id"].as_str().unwrap())["output"],
            "|||explicit-value|session-value"
        );
        println!("environment-helper-completed");
    }
}
