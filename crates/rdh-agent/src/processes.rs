//! POSIX shell jobs with one supervisor and a pinned, unreaped group leader.
//!
//! Process groups do not confine commands which deliberately create a new
//! session/group. Directory descriptors avoid reopening a checked path, but a
//! same-user actor can still move that directory after validation. Shells retain
//! the OS user's authority; file roots are not a shell sandbox.
use crate::{config::DeviceConfig, files::Files, sandbox::Sandbox};
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

#[cfg(any(
    all(test, unix),
    all(target_os = "macos", feature = "experimental-macos-processes")
))]
#[path = "processes_macos.rs"]
mod macos;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes")
))]
pub const PROCESS_OPERATIONS: &[&str] = &[
    "start_process",
    "read_process",
    "list_processes",
    "write_process_stdin",
    "terminate_process",
];
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes")
)))]
pub const PROCESS_OPERATIONS: &[&str] = &[];

pub type ExitHook = Arc<dyn Fn(String, Value) + Send + Sync>;

#[derive(Debug)]
pub struct ProcessFailure {
    pub code: &'static str,
    pub message: String,
    pub state: &'static str,
    pub summary: Value,
}

impl ProcessFailure {
    fn new(code: &'static str, message: impl Into<String>, state: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            state,
            summary: serde_json::json!({}),
        }
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes")
))]
pub use posix::Processes;

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes")
)))]
pub struct Processes;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes")
)))]
impl Processes {
    pub fn new(
        _: &DeviceConfig,
        _: Arc<Files>,
        _: Vec<String>,
        _: Option<ExitHook>,
    ) -> Result<Self, String> {
        Ok(Self)
    }
    pub fn with_sandbox(
        _: &DeviceConfig,
        _: Arc<Files>,
        _: Vec<String>,
        _: Option<ExitHook>,
        _: Arc<Sandbox>,
    ) -> Result<Self, String> {
        Ok(Self)
    }
    pub fn dispatch(
        &self,
        _: &str,
        _: &Value,
        _: &str,
        _: Instant,
        _: &AtomicBool,
    ) -> Result<Value, ProcessFailure> {
        Err(ProcessFailure::new(
            "unsupported",
            "Native process supervision is unsupported on this platform",
            "not_sent",
        ))
    }
    pub fn mark_offline(&self) {}
    pub fn mark_online(&self) {}
    pub fn stop_offline_jobs(&self) {}
    pub fn quiescent(&self) -> bool {
        true
    }
    pub fn close(&self, _: Duration) -> bool {
        true
    }
}

#[cfg(all(
    test,
    target_os = "macos",
    not(feature = "experimental-macos-processes")
))]
mod macos_default_tests {
    use super::*;

    #[test]
    fn process_operations_are_disabled_without_native_validation_feature() {
        assert!(PROCESS_OPERATIONS.is_empty());
        let error = Processes
            .dispatch(
                "start_process",
                &serde_json::json!({"root":"work","command":"touch forbidden"}),
                "default-gate",
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(error.code, "unsupported");
        assert_eq!(error.state, "not_sent");
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes")
))]
mod posix {
    #[cfg(target_os = "macos")]
    use super::macos::group_has_live_members;
    use super::*;
    use serde_json::json;
    #[cfg(any(target_os = "linux", test))]
    use std::fs;
    use std::{
        collections::{BTreeMap, VecDeque},
        ffi::OsString,
        io::{self, Read, Write},
        os::{
            fd::AsRawFd,
            unix::fs::MetadataExt,
            unix::process::{CommandExt, ExitStatusExt},
        },
        process::{Child, ChildStdin, Command, Stdio},
        sync::{Condvar, Mutex, MutexGuard, TryLockError, atomic::Ordering, mpsc},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(target_os = "linux")]
    const GROUP_CHECK: &str = "procfs_cleanup_check";
    #[cfg(target_os = "macos")]
    const GROUP_CHECK: &str = "process_group_cleanup_check";
    const MAX_OUTPUT: usize = 256 * 1024;
    const MAX_JOBS: usize = 64;
    const TICK: Duration = Duration::from_millis(5);
    const SAFE_INTEGER: u64 = 9_007_199_254_740_991;

    // Each element is one Unicode scalar value. The allocation is bounded by
    // MAX_OUTPUT * size_of::<char>(), independently of UTF-8 byte length.
    #[derive(Default)]
    struct Output {
        chars: VecDeque<char>,
        base: u64,
        bytes: usize,
    }
    impl Output {
        fn append(&mut self, text: &str) {
            for ch in text.chars() {
                if self.chars.len() == MAX_OUTPUT {
                    self.bytes -= self.chars.pop_front().unwrap().len_utf8();
                    self.base += 1;
                }
                self.chars.push_back(ch);
                self.bytes += ch.len_utf8();
            }
        }
    }

    #[derive(Default)]
    struct Decoder {
        pending: Vec<u8>,
    }
    impl Decoder {
        fn decode(&mut self, bytes: &[u8], end: bool) -> String {
            self.pending.extend_from_slice(bytes);
            let mut result = String::new();
            let mut consumed = 0;
            while consumed < self.pending.len() {
                match std::str::from_utf8(&self.pending[consumed..]) {
                    Ok(text) => {
                        result.push_str(text);
                        consumed = self.pending.len();
                    }
                    Err(error) => {
                        let valid = error.valid_up_to();
                        // The UTF-8 validator proves this prefix is valid.
                        result.push_str(
                            std::str::from_utf8(&self.pending[consumed..consumed + valid]).unwrap(),
                        );
                        consumed += valid;
                        match error.error_len() {
                            Some(length) => {
                                result.push('\u{fffd}');
                                consumed += length;
                            }
                            None if end => {
                                result.push('\u{fffd}');
                                consumed = self.pending.len();
                            }
                            None => break,
                        }
                    }
                }
            }
            self.pending.drain(..consumed);
            result
        }
    }

    struct State {
        pid: Option<u32>,
        status: &'static str,
        exit_code: Option<i32>,
        signal: Option<i32>,
        error: Option<Value>,
        started: f64,
        output: Output,
        ready: bool,
        closed: bool,
    }
    struct Job {
        id: String,
        request_id: String,
        state: Mutex<State>,
        changed: Condvar,
        stop: AtomicBool,
        stdin: Mutex<Option<ChildStdin>>,
        stdin_serial: Mutex<()>,
    }
    impl Job {
        fn new(id: String, request_id: &str) -> Self {
            Self {
                id,
                request_id: request_id.into(),
                state: Mutex::new(State {
                    pid: None,
                    status: "running",
                    exit_code: None,
                    signal: None,
                    error: None,
                    started: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs_f64(),
                    output: Output::default(),
                    ready: false,
                    closed: false,
                }),
                changed: Condvar::new(),
                stop: AtomicBool::new(false),
                stdin: Mutex::new(None),
                stdin_serial: Mutex::new(()),
            }
        }
        fn read(&self, offset: f64, limit: usize, output: bool) -> Value {
            let state = lock(&self.state);
            let mut result = json!({
                "job_id":self.id, "pid":state.pid, "status":state.status,
                "exit_code":state.exit_code, "started_at":state.started,
                "cleanup_pending":state.status != "running" && !state.closed,
            });
            if let Some(error) = &state.error {
                result["error"] = error.clone();
            }
            if output {
                let base = state.output.base as f64;
                let start = offset.max(base);
                // Float-to-usize conversion saturates for an offset beyond the
                // retained buffer. That lets all finite integral JS Numbers
                // produce the same empty page without allocating or looping.
                let relative = (start - base) as usize;
                let content: String = state
                    .output
                    .chars
                    .iter()
                    .skip(relative)
                    .take(limit)
                    .collect();
                let cursor = |value: f64| {
                    if value <= SAFE_INTEGER as f64 {
                        json!(value as u64)
                    } else {
                        json!(value)
                    }
                };
                result["offset"] = cursor(start);
                result["next_offset"] = cursor(start + content.chars().count() as f64);
                result["output_dropped"] = json!(offset < base);
                result["output"] = json!(content);
            }
            result
        }
        fn terminate(&self) {
            let mut state = lock(&self.state);
            if !state.closed {
                if state.status == "running" {
                    state.status = "terminated";
                }
                self.stop.store(true, Ordering::Release);
                self.changed.notify_all();
            }
        }
    }

    struct Registry {
        accepting: bool,
        closing: bool,
        starts: usize,
        jobs: VecDeque<Arc<Job>>,
    }
    struct Inner {
        config: DeviceConfig,
        files: Arc<Files>,
        sandbox: Arc<Sandbox>,
        inherit_env: Vec<String>,
        on_exit: Option<ExitHook>,
        registry: Mutex<Registry>,
    }
    pub struct Processes {
        inner: Arc<Inner>,
    }
    struct Starting(Arc<Inner>);
    impl Drop for Starting {
        fn drop(&mut self) {
            lock(&self.0.registry).starts -= 1;
        }
    }

    // Poison is recovered only to retain access to owned jobs for cleanup.
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|error| error.into_inner())
    }
    fn check(deadline: Instant, closing: &AtomicBool) -> Result<(), ProcessFailure> {
        if closing.load(Ordering::Acquire) {
            return Err(ProcessFailure::new(
                "operation_busy",
                "Agent is shutting down; process operation was not started",
                "not_sent",
            ));
        }
        if Instant::now() >= deadline {
            return Err(ProcessFailure::new(
                "operation_timeout",
                "Process operation deadline expired before execution",
                "not_sent",
            ));
        }
        Ok(())
    }
    fn string<'a>(
        params: &'a Value,
        key: &str,
        default: Option<&'a str>,
    ) -> Result<&'a str, ProcessFailure> {
        match params.get(key) {
            None => default.ok_or_else(|| invalid(format!("Missing {key}"))),
            Some(value) => value
                .as_str()
                .ok_or_else(|| invalid(format!("Invalid {key}"))),
        }
    }
    fn invalid(message: impl Into<String>) -> ProcessFailure {
        ProcessFailure::new("invalid_params", message, "not_sent")
    }
    fn integer(
        params: &Value,
        key: &str,
        default: u64,
        min: u64,
        max: u64,
    ) -> Result<u64, ProcessFailure> {
        match params.get(key) {
            None => Ok(default),
            Some(value) => value
                .as_f64()
                .filter(|n| {
                    n.is_finite() && n.fract() == 0.0 && *n >= min as f64 && *n <= max as f64
                })
                .map(|n| n as u64)
                .ok_or_else(|| invalid(format!("Invalid {key}"))),
        }
    }
    fn os_error(error: &io::Error, syscall: &str) -> Value {
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

    impl Processes {
        /// Finished jobs are idle only after their pipes and exit hooks close.
        pub fn quiescent(&self) -> bool {
            self.inner.registry.try_lock().is_ok_and(|registry| {
                registry.starts == 0
                    && registry
                        .jobs
                        .iter()
                        .all(|job| job.state.try_lock().is_ok_and(|state| state.closed))
            })
        }
        pub fn new(
            config: &DeviceConfig,
            files: Arc<Files>,
            inherit_env: Vec<String>,
            on_exit: Option<ExitHook>,
        ) -> Result<Self, String> {
            let sandbox = Sandbox::prepare(config, &crate::policy::Policy::default(), None)
                .map_err(|error| error.to_string())?;
            Self::with_sandbox(config, files, inherit_env, on_exit, Arc::new(sandbox))
        }

        pub fn with_sandbox(
            config: &DeviceConfig,
            files: Arc<Files>,
            inherit_env: Vec<String>,
            on_exit: Option<ExitHook>,
            sandbox: Arc<Sandbox>,
        ) -> Result<Self, String> {
            if inherit_env.len() > 64
                || inherit_env.iter().any(|name| {
                    name.is_empty()
                        || name.len() > 128
                        || !name.bytes().enumerate().all(|(i, b)| {
                            b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                        })
                })
            {
                return Err(
                    "inherit_env must contain at most 64 valid environment variable names".into(),
                );
            }
            Ok(Self {
                inner: Arc::new(Inner {
                    config: config.clone(),
                    files,
                    sandbox,
                    inherit_env,
                    on_exit,
                    registry: Mutex::new(Registry {
                        accepting: true,
                        closing: false,
                        starts: 0,
                        jobs: VecDeque::new(),
                    }),
                }),
            })
        }

        pub fn dispatch(
            &self,
            operation: &str,
            params: &Value,
            request_id: &str,
            deadline: Instant,
            closing: &AtomicBool,
        ) -> Result<Value, ProcessFailure> {
            if !params.is_object() {
                return Err(invalid("Process parameters must be an object"));
            }
            match operation {
                "start_process" => self.start(params, request_id, deadline, closing),
                "read_process" => {
                    let job = self.job(params)?;
                    let offset = match params.get("offset") {
                        None => 0.0,
                        Some(value) => value
                            .as_f64()
                            .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 0.0)
                            .ok_or_else(|| invalid("Invalid offset/limit"))?,
                    };
                    let limit = integer(params, "limit", 32768, 1, 65536)
                        .map_err(|_| invalid("Invalid offset/limit"))?
                        as usize;
                    Ok(job.read(offset, limit, true))
                }
                "list_processes" => {
                    let jobs = lock(&self.inner.registry)
                        .jobs
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>();
                    Ok(
                        json!({"jobs":jobs.iter().map(|job| job.read(0.0, 0, false)).collect::<Vec<_>>()}),
                    )
                }
                "write_process_stdin" => self.write_stdin(params, deadline, closing),
                "terminate_process" => {
                    let job = self.job(params)?;
                    check(deadline, closing)?;
                    job.terminate();
                    wait_closed(&job, deadline.min(Instant::now() + Duration::from_secs(1)));
                    Ok(job.read(0.0, 32768, true))
                }
                _ => Err(ProcessFailure::new(
                    "unsupported",
                    "Unknown process operation",
                    "not_sent",
                )),
            }
        }

        fn job(&self, params: &Value) -> Result<Arc<Job>, ProcessFailure> {
            let id = string(params, "job_id", None)?;
            lock(&self.inner.registry)
                .jobs
                .iter()
                .find(|job| job.id == id)
                .cloned()
                .ok_or_else(|| invalid("Unknown process job"))
        }

        fn start(
            &self,
            params: &Value,
            request_id: &str,
            deadline: Instant,
            closing: &AtomicBool,
        ) -> Result<Value, ProcessFailure> {
            if !self.inner.config.allow_shell {
                return Err(ProcessFailure::new(
                    "policy_denied",
                    "Shell execution is disabled in this device's configuration",
                    "not_sent",
                ));
            }
            let command = string(params, "command", None)?;
            if command.is_empty()
                || command.encode_utf16().count() > 32768
                || command.contains('\0')
            {
                return Err(invalid("Invalid command"));
            }
            let root = string(params, "root", None)?;
            let cwd = string(params, "cwd", Some("."))?;
            let timeout = integer(
                params,
                "timeout_seconds",
                u64::from(self.inner.config.max_process_seconds.min(300)),
                1,
                u64::from(self.inner.config.max_process_seconds),
            )?;
            let wait = match params.get("wait_seconds") {
                None => 1.0,
                Some(value) => value
                    .as_f64()
                    .filter(|n| n.is_finite() && (0.0..=5.0).contains(n))
                    .ok_or_else(|| invalid("Invalid wait_seconds"))?,
            };
            check(deadline, closing)?;
            {
                let mut registry = lock(&self.inner.registry);
                if !registry.accepting || registry.closing {
                    return Err(ProcessFailure::new(
                        "operation_busy",
                        "Process manager is closing or offline",
                        "not_sent",
                    ));
                }
                registry.starts += 1;
            }
            let _starting = Starting(Arc::clone(&self.inner));
            let directory = self
                .inner
                .files
                .working_directory(root, cwd)
                .map_err(invalid)?;
            let workdir = std::path::Path::new(
                self.inner
                    .config
                    .roots
                    .get(root)
                    .ok_or_else(|| invalid("Unknown root"))?,
            )
            .join(cwd);
            if self.inner.sandbox.is_isolated() {
                let named =
                    std::fs::metadata(&workdir).map_err(|error| invalid(error.to_string()))?;
                let held = directory
                    .metadata()
                    .map_err(|error| invalid(error.to_string()))?;
                if named.dev() != held.dev() || named.ino() != held.ino() {
                    return Err(invalid("Working directory changed while preparing sandbox"));
                }
            }
            let (executable, arguments) = self
                .inner
                .sandbox
                .command(&workdir, command)
                .map_err(|error| invalid(error.to_string()))?;
            let env = environment(&self.inner.inherit_env, !self.inner.sandbox.is_isolated());
            let id =
                crate::random::hex_token::<12>().map_err(|error| invalid(error.to_string()))?;
            let job = Arc::new(Job::new(id, request_id));
            {
                let mut registry = lock(&self.inner.registry);
                check(deadline, closing)?;
                if !registry.accepting || registry.closing {
                    return Err(ProcessFailure::new(
                        "operation_busy",
                        "Process manager is closing or offline",
                        "not_sent",
                    ));
                }
                if registry
                    .jobs
                    .iter()
                    .filter(|job| !lock(&job.state).closed)
                    .count()
                    >= self.inner.config.max_processes as usize
                {
                    return Err(ProcessFailure::new(
                        "operation_busy",
                        "Maximum concurrent processes reached",
                        "not_sent",
                    ));
                }
                if registry.jobs.len() >= MAX_JOBS {
                    let old = registry
                        .jobs
                        .iter()
                        .position(|job| lock(&job.state).closed)
                        .ok_or_else(|| {
                            ProcessFailure::new(
                                "operation_busy",
                                "Maximum retained process jobs reached",
                                "not_sent",
                            )
                        })?;
                    registry.jobs.remove(old);
                }
                registry.jobs.push_back(Arc::clone(&job));
            }
            // Allocate the supervisor before fork. A thread creation failure must
            // never leave a live child without an owner capable of reaping it.
            let (sender, receiver) = mpsc::channel::<Result<Child, io::Error>>();
            let watched = Arc::clone(&job);
            let hook = self.inner.on_exit.clone();
            if let Err(error) = thread::Builder::new()
                .name("rdh-process".into())
                .spawn(move || match receiver.recv() {
                    Ok(Ok(child)) => {
                        supervise(&watched, child, Duration::from_secs(timeout), hook.as_ref())
                    }
                    Ok(Err(error)) => finish_spawn_failure(&watched, error, hook.as_ref()),
                    Err(_) => finish_spawn_failure(
                        &watched,
                        io::Error::from_raw_os_error(libc::ECANCELED),
                        hook.as_ref(),
                    ),
                })
            {
                finish_spawn_failure(&job, error, self.inner.on_exit.as_ref());
                return Err(start_failure(&job));
            }
            let mut child_command = Command::new(executable);
            child_command
                .args(arguments)
                .env_clear()
                .envs(env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let directory_fd = directory.as_raw_fd();
            let pre_exec_job = Arc::clone(&job);
            // Only async-signal-safe syscalls/atomic loads run between fork and exec.
            // The parent owns directory until spawn returns; no pathname is reopened.
            unsafe {
                child_command.pre_exec(move || {
                    if pre_exec_job.stop.load(Ordering::Acquire) {
                        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                    }
                    if libc::setsid() == -1 || libc::fchdir(directory_fd) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            // No admission lock is held during fork/exec or the optional wait.
            if let Err(error) = check(deadline, closing) {
                let _ = sender.send(Err(io::Error::from_raw_os_error(libc::ECANCELED)));
                return Err(error);
            }
            if job.stop.load(Ordering::Acquire) {
                let _ = sender.send(Err(io::Error::from_raw_os_error(libc::ECANCELED)));
                return Err(ProcessFailure::new(
                    "operation_busy",
                    "Process manager is closing; launch was cancelled",
                    "not_sent",
                ));
            }
            let launched = child_command.spawn();
            // The receiver cannot disappear normally before receiving this child.
            // If it does, retain ownership and clean up synchronously.
            if let Err(error) = sender.send(launched) {
                match error.0 {
                    Ok(child) => {
                        job.terminate();
                        supervise(&job, child, Duration::ZERO, self.inner.on_exit.as_ref());
                    }
                    Err(error) => finish_spawn_failure(&job, error, self.inner.on_exit.as_ref()),
                }
            }
            let mut state = lock(&job.state);
            while !state.ready && Instant::now() < deadline {
                state = job
                    .changed
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(|error| error.into_inner())
                    .0;
            }
            if !state.ready {
                drop(state);
                let mut failure = ProcessFailure::new(
                    "operation_timeout",
                    "Process launch outcome is unknown; inspect this job before retrying",
                    "unknown",
                );
                failure.summary = job.read(0.0, 0, false);
                return Err(failure);
            }
            if state.status == "failed" {
                drop(state);
                return Err(start_failure(&job));
            }
            drop(state);
            wait_closed(
                &job,
                deadline.min(Instant::now() + Duration::from_secs_f64(wait)),
            );
            Ok(job.read(0.0, 32768, true))
        }

        fn write_stdin(
            &self,
            params: &Value,
            deadline: Instant,
            closing: &AtomicBool,
        ) -> Result<Value, ProcessFailure> {
            let job = self.job(params)?;
            let text = string(params, "text", Some(""))?;
            let close = match params.get("close") {
                None => false,
                Some(value) => value.as_bool().ok_or_else(|| invalid("Invalid close"))?,
            };
            if text.len() > 16384 {
                return Err(invalid("Process input exceeds 16 KiB"));
            }
            let expires = deadline.min(Instant::now() + Duration::from_secs(3));
            let mut written = 0;
            let serial = loop {
                input_check(&job, written, expires, closing)?;
                match job.stdin_serial.try_lock() {
                    Ok(guard) => break guard,
                    Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                    Err(TryLockError::WouldBlock) => thread::sleep(TICK),
                }
            };
            loop {
                input_check(&job, written, expires, closing)?;
                let mut stdin = lock(&job.stdin);
                if lock(&job.state).status != "running" || stdin.is_none() {
                    return Err(input_failure(
                        &job,
                        written,
                        "Process is not running or stdin is closed",
                        "process_stdin_closed",
                    ));
                }
                // Recheck after waiting for both locks, immediately before write/close.
                input_check(&job, written, expires, closing)?;
                if written == text.len() {
                    if close {
                        stdin.take();
                    }
                    break;
                }
                match stdin.as_mut().unwrap().write(&text.as_bytes()[written..]) {
                    Ok(0) => {
                        return Err(input_failure(
                            &job,
                            written,
                            "Process stdin closed during write",
                            "process_stdin_closed",
                        ));
                    }
                    Ok(count) => written += count,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        drop(stdin);
                        thread::sleep(TICK);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        return Err(input_failure(
                            &job,
                            written,
                            &format!(
                                "Process stdin write failed: {}",
                                os_error(&error, "write")["code"]
                            ),
                            "process_stdin_failed",
                        ));
                    }
                }
            }
            drop(serial);
            Ok(json!({"job_id":job.id, "sent":text.chars().count(), "stdin_closed":close}))
        }

        pub fn mark_offline(&self) {
            lock(&self.inner.registry).accepting = false;
        }
        pub fn mark_online(&self) {
            let mut registry = lock(&self.inner.registry);
            if !registry.closing {
                registry.accepting = true;
            }
        }
        /// Called only if the same outage still owns the connection state.
        /// Unlike final shutdown, this leaves the manager reusable after ready.
        pub fn stop_offline_jobs(&self) {
            let registry = lock(&self.inner.registry);
            if !registry.accepting && !registry.closing {
                for job in &registry.jobs {
                    job.terminate();
                }
            }
        }
        pub fn close(&self, timeout: Duration) -> bool {
            let expires = Instant::now() + timeout;
            {
                let mut registry = lock(&self.inner.registry);
                registry.closing = true;
                registry.accepting = false;
                for job in &registry.jobs {
                    job.terminate();
                }
            }
            loop {
                let registry = lock(&self.inner.registry);
                if registry.starts == 0 && registry.jobs.iter().all(|job| lock(&job.state).closed) {
                    return true;
                }
                drop(registry);
                if Instant::now() >= expires {
                    return false;
                }
                thread::sleep(TICK.min(expires.saturating_duration_since(Instant::now())));
            }
        }
    }
    impl Drop for Processes {
        fn drop(&mut self) {
            let _ = self.close(Duration::ZERO);
        }
    }

    fn wait_closed(job: &Job, expires: Instant) {
        let mut state = lock(&job.state);
        while !state.closed && Instant::now() < expires {
            let remaining = expires.saturating_duration_since(Instant::now());
            state = job
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
    }
    fn input_failure(
        job: &Job,
        written: usize,
        message: &str,
        code: &'static str,
    ) -> ProcessFailure {
        let mut failure = ProcessFailure::new(
            code,
            if written > 0 {
                format!(
                    "{message}; input outcome is unknown after a partial write; inspect before retrying"
                )
            } else {
                message.into()
            },
            if written > 0 { "unknown" } else { "not_sent" },
        );
        failure.summary = json!({"job_id":job.id, "bytes_written":written});
        failure
    }
    fn input_check(
        job: &Job,
        written: usize,
        expires: Instant,
        closing: &AtomicBool,
    ) -> Result<(), ProcessFailure> {
        if closing.load(Ordering::Acquire) || job.stop.load(Ordering::Acquire) {
            return Err(input_failure(
                job,
                written,
                "Process stdin cancelled while shutting down",
                "operation_busy",
            ));
        }
        if Instant::now() >= expires {
            return Err(input_failure(
                job,
                written,
                "Process stdin did not drain within 3 seconds or the request deadline expired",
                "operation_timeout",
            ));
        }
        Ok(())
    }
    fn start_failure(job: &Job) -> ProcessFailure {
        let launched = lock(&job.state).pid.is_some();
        let mut failure = ProcessFailure::new(
            "process_start_failed",
            if launched {
                format!(
                    "Process supervisor setup failed after launch; outcome is unknown; job_id={}",
                    job.id
                )
            } else {
                format!("Process launch failed; job_id={}", job.id)
            },
            if launched { "unknown" } else { "failed" },
        );
        failure.summary = job.read(0.0, 0, false);
        failure
    }
    fn finish_spawn_failure(job: &Job, error: io::Error, hook: Option<&ExitHook>) {
        {
            let mut state = lock(&job.state);
            state.status = "failed";
            state.error = Some(os_error(&error, "spawn"));
            state.ready = true;
            job.changed.notify_all();
        }
        finish(job, hook);
    }
    fn finish(job: &Job, hook: Option<&ExitHook>) {
        // Hooks complete before the resource permit is released. In particular,
        // close cannot report success while a completion audit is still running.
        if let Some(hook) = hook {
            let mut summary = job.read(0.0, 0, false);
            summary["cleanup_pending"] = json!(false);
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook(job.request_id.clone(), summary)
            }))
            .is_err()
            {
                lock(&job.state).error = Some(json!({"name":"Error", "code":"EXIT_HOOK_FAILED"}));
                return;
            }
        }
        lock(&job.state).closed = true;
        job.changed.notify_all();
    }

    fn nonblocking(fd: i32) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
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
    fn drain<R: Read>(stream: &mut Option<R>, decoder: &mut Decoder, job: &Job) -> io::Result<()> {
        let mut bytes = [0u8; 8192];
        // Yield to timeout/termination checks even for an infinite output writer.
        for _ in 0..16 {
            let Some(reader) = stream.as_mut() else { break };
            match reader.read(&mut bytes) {
                Ok(0) => {
                    lock(&job.state).output.append(&decoder.decode(&[], true));
                    stream.take();
                    break;
                }
                Ok(count) => lock(&job.state)
                    .output
                    .append(&decoder.decode(&bytes[..count], false)),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn supervise(job: &Job, mut child: Child, timeout: Duration, hook: Option<&ExitHook>) {
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
                state.exit_code = code;
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
                                    state.exit_code = status.code();
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
            let error = fixture.call("start_process", json!({"root":"work","command":"private command and credential","wait_seconds":0})).unwrap_err();
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
            let fixture =
                Fixture::configured(|config| config.max_process_seconds = 1, None, vec![]);
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
            let fixture =
                Fixture::configured(|config| config.max_processes = 1, Some(hook), vec![]);
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
                    "processes::posix::tests::environment_helper",
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
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("environment-helper-completed")
            );
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
}
