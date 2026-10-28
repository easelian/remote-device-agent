//! Bounded process protocol and ownership shared by the platform supervisors.
use super::*;
use serde_json::json;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{Condvar, Mutex, MutexGuard, TryLockError, atomic::Ordering, mpsc},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
#[path = "processes_posix.rs"]
mod platform;
#[cfg(windows)]
#[path = "processes_windows.rs"]
pub(super) mod platform;
use platform::{Child, os_error, supervise};

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
    exit_code: Option<i64>,
    #[cfg(unix)]
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
    stdin: Mutex<Option<platform::Input>>,
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
                #[cfg(unix)]
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
            .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= min as f64 && *n <= max as f64)
            .map(|n| n as u64)
            .ok_or_else(|| invalid(format!("Invalid {key}"))),
    }
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
        if command.is_empty() || command.encode_utf16().count() > 32768 || command.contains('\0') {
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
        let plan = platform::prepare(&self.inner, root, cwd, command, deadline)?;
        let id = crate::random::hex_token::<12>().map_err(|error| invalid(error.to_string()))?;
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
        // Allocate the supervisor before creating a child. Thread creation failure
        // must not leave a process without an owner capable of cleaning it up.
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
                Err(_) => {
                    finish_spawn_failure(&watched, platform::cancelled_error(), hook.as_ref())
                }
            })
        {
            finish_spawn_failure(&job, error, self.inner.on_exit.as_ref());
            return Err(start_failure(&job));
        }
        // No admission lock is held during process creation or the optional wait.
        if let Err(error) = check(deadline, closing) {
            let _ = sender.send(Err(platform::cancelled_error()));
            return Err(error);
        }
        if job.stop.load(Ordering::Acquire) {
            let _ = sender.send(Err(platform::cancelled_error()));
            return Err(ProcessFailure::new(
                "operation_busy",
                "Process manager is closing; launch was cancelled",
                "not_sent",
            ));
        }
        let launched = platform::spawn(plan, &job);
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
fn input_failure(job: &Job, written: usize, message: &str, code: &'static str) -> ProcessFailure {
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
