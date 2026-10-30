//! Admission and execution live longer than any particular WebSocket session.
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

use crate::audit::{AuditLog, AuditRecord};
use crate::config::{DeviceConfig, sha256_hex};
use crate::error::{Error, Result};
use crate::files::{FILE_OPERATIONS, Files, MUTATION_DEADLINE_EXPIRED, MUTATION_SHUTTING_DOWN};
use crate::policy::Policy;
use crate::processes::{ExitHook, PROCESS_OPERATIONS, Processes};
use crate::receipts::{Begin, ReceiptError, Receipts, result_summary};
use crate::sandbox::Sandbox;
use crate::searches::{SEARCH_OPERATIONS, SHUTTING_DOWN, START_EXPIRED, Searches};
use crate::startup::{Mode, STARTUP_OPERATIONS, Startup};
use crate::system;
use crate::transport::{MAX_FRAME, MAX_QUEUED_BYTES, Sender};

const ORDINARY_LIMIT: usize = 8;
const RESPONSE_DEADLINE: Duration = Duration::from_secs(20);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(6);
const OFFLINE_GRACE: Duration = Duration::from_secs(300);
const NULL: Value = Value::Null;

#[derive(Clone)]
struct Failure {
    code: String,
    message: String,
    state: &'static str,
    summary: Value,
}

impl Failure {
    fn new(code: impl Into<String>, message: impl Into<String>, state: &'static str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            state,
            summary: json!({}),
        }
    }
    fn receipt(error: ReceiptError) -> Self {
        Self {
            code: error.code.into(),
            message: error.message.into(),
            state: error.state,
            summary: error.summary,
        }
    }
}

enum Outcome {
    Success(Value),
    Failure(Failure),
}

struct Request {
    id: String,
    operation: String,
    params: Value,
    deadline: Instant,
    receipt_id: Option<String>,
}

pub type ActiveIds = Arc<Mutex<HashSet<String>>>;

#[derive(Debug)]
pub struct InvalidRequest;

struct Flight {
    runtime: Arc<Runtime>,
    id: String,
    active_ids: ActiveIds,
    _permit: OwnedSemaphorePermit,
}

struct DiagnosticFlight {
    runtime: Arc<Runtime>,
    _permit: OwnedSemaphorePermit,
}

struct ResponseFlight(Arc<Runtime>);
impl Drop for ResponseFlight {
    fn drop(&mut self) {
        self.0.responses.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Holds every admission permit until the session has stopped accepting work.
#[cfg(target_os = "linux")]
pub(crate) struct IdleGuard {
    _ordinary: OwnedSemaphorePermit,
    _status: OwnedSemaphorePermit,
    _diagnostic: OwnedSemaphorePermit,
}

#[cfg(test)]
#[derive(Default)]
struct CompletionGate {
    entered: Notify,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
}

#[cfg(test)]
impl CompletionGate {
    fn wait(&self) {
        let released = self.released.lock().unwrap();
        self.entered.notify_one();
        let (released, timeout) = self
            .wake
            .wait_timeout_while(released, Duration::from_secs(30), |released| !*released)
            .unwrap();
        assert!(
            *released && !timeout.timed_out(),
            "completion gate timed out"
        );
    }

    #[cfg(any(
        target_os = "linux",
        all(
            target_os = "macos",
            feature = "experimental-macos-files",
            feature = "experimental-macos-processes",
            feature = "experimental-macos-audit"
        )
    ))]
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

impl Drop for DiagnosticFlight {
    fn drop(&mut self) {
        self.runtime.active.fetch_sub(1, Ordering::AcqRel);
        self.runtime.finished.notify_one();
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Ok(mut ids) = self.active_ids.lock() {
            ids.remove(&self.id);
        }
        self.runtime.active.fetch_sub(1, Ordering::AcqRel);
        self.runtime.finished.notify_one();
    }
}

pub struct Runtime {
    config: DeviceConfig,
    policy: Policy,
    files: Arc<Files>,
    searches: Arc<Searches>,
    processes: Arc<Processes>,
    sandbox: Arc<Sandbox>,
    startup: Option<Arc<Startup>>,
    audit: Option<Arc<AuditLog>>,
    pub receipts: Receipts,
    ordinary: Arc<Semaphore>,
    status: Arc<Semaphore>,
    diagnostic_audit: Arc<Semaphore>,
    closing: AtomicBool,
    quarantined: AtomicBool,
    active: AtomicUsize,
    responses: AtomicUsize,
    finished: Notify,
    generation: AtomicU64,
    offline_epoch: AtomicU64,
    connection: Mutex<Option<(u64, Sender)>>,
    #[cfg(test)]
    completion_gate: Mutex<Option<Arc<CompletionGate>>>,
}

impl Runtime {
    pub fn new(
        config: DeviceConfig,
        policy: Policy,
        audit: Option<Arc<AuditLog>>,
    ) -> Result<Arc<Self>> {
        Self::with_startup(config, policy, audit, None)
    }

    pub fn with_startup(
        config: DeviceConfig,
        policy: Policy,
        audit: Option<Arc<AuditLog>>,
        startup: Option<Arc<Startup>>,
    ) -> Result<Arc<Self>> {
        Self::with_config_path(config, policy, audit, startup, None)
    }

    pub fn with_config_path(
        config: DeviceConfig,
        policy: Policy,
        audit: Option<Arc<AuditLog>>,
        startup: Option<Arc<Startup>>,
        config_path: Option<&std::path::Path>,
    ) -> Result<Arc<Self>> {
        let files =
            Arc::new(Files::new(&config.roots, config.allow_write).map_err(Error::message)?);
        let searches = Searches::new(Arc::clone(&files));
        let sandbox = Arc::new(Sandbox::prepare(&config, &policy, config_path)?);
        let exit_hook: Option<ExitHook> = audit.as_ref().map(|audit| {
            let audit = Arc::clone(audit);
            Arc::new(move |id: String, result: Value| {
                let started_ms = result
                    .get("started_at")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * 1000.0;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64()
                    * 1000.0;
                let duration_ms = (now_ms - started_ms).max(0.0).min(u64::MAX as f64) as u64;
                let status = result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let error = result
                    .get("error")
                    .filter(|value| !value.is_null())
                    .map(Value::to_string);
                if audit
                    .record(AuditRecord {
                        id: Some(id),
                        operation: "process_exit".into(),
                        status,
                        duration_ms,
                        params: json!({}),
                        result: Some(result),
                        error,
                    })
                    .is_err()
                {
                    eprintln!(
                        "Local audit could not record process completion; inspect audit health"
                    );
                }
            }) as ExitHook
        });
        let processes = Arc::new(
            Processes::with_sandbox(
                &config,
                Arc::clone(&files),
                policy.inherit_env.clone(),
                exit_hook,
                Arc::clone(&sandbox),
            )
            .map_err(Error::message)?,
        );
        let receipts = Receipts::new(&config.token)?;
        Ok(Arc::new(Self {
            config,
            policy,
            files,
            searches,
            processes,
            sandbox,
            startup,
            audit,
            receipts,
            ordinary: Arc::new(Semaphore::new(ORDINARY_LIMIT)),
            status: Arc::new(Semaphore::new(1)),
            diagnostic_audit: Arc::new(Semaphore::new(1)),
            closing: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            responses: AtomicUsize::new(0),
            finished: Notify::new(),
            generation: AtomicU64::new(0),
            offline_epoch: AtomicU64::new(0),
            connection: Mutex::new(None),
            #[cfg(test)]
            completion_gate: Mutex::new(None),
        }))
    }

    pub fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Called before connecting a service-update trial to the Hub. Activation
    /// is one-way: once requests can run, a controller must roll forward.
    #[cfg(target_os = "linux")]
    pub(crate) fn quarantine_before_connect(&self) {
        self.quarantined.store(true, Ordering::Release);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn is_quarantined(&self) -> bool {
        self.quarantined.load(Ordering::Acquire)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn activate_trial(&self) {
        self.quarantined.store(false, Ordering::Release);
    }

    /// Freeze admission before examining all remaining owners. If any owner
    /// cannot prove itself idle, dropping the permits restores normal service.
    #[cfg(target_os = "linux")]
    pub(crate) fn try_freeze_idle(&self) -> Option<IdleGuard> {
        self.try_freeze(false)
    }

    /// A trial that has never admitted work can also stop while offline. The
    /// reporter checks trial identity and serializes this with activation.
    #[cfg(target_os = "linux")]
    pub(crate) fn try_freeze_trial(&self) -> Option<IdleGuard> {
        if !self.is_quarantined() {
            return None;
        }
        self.try_freeze(true)
    }

    #[cfg(target_os = "linux")]
    fn try_freeze(&self, offline_trial: bool) -> Option<IdleGuard> {
        if self.closing.load(Ordering::Acquire) {
            return None;
        }
        let ordinary = Arc::clone(&self.ordinary)
            .try_acquire_many_owned(ORDINARY_LIMIT as u32)
            .ok()?;
        let status = Arc::clone(&self.status).try_acquire_owned().ok()?;
        let diagnostic = Arc::clone(&self.diagnostic_audit)
            .try_acquire_owned()
            .ok()?;
        if self.closing.load(Ordering::Acquire)
            || self.active.load(Ordering::Acquire) != 0
            || self.responses.load(Ordering::Acquire) != 0
            || !self.processes.quiescent()
            || !self.searches.quiescent()
            || !self.receipts.quiescent()
        {
            return None;
        }
        let connection = self.connection.try_lock().ok()?;
        let transport_idle = match connection.as_ref() {
            Some((_, sender)) => sender.idle(),
            None => offline_trial && self.is_quarantined(),
        };
        if !transport_idle {
            return None;
        }
        drop(connection);
        Some(IdleGuard {
            _ordinary: ordinary,
            _status: status,
            _diagnostic: diagnostic,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn begin_shutdown(&self) {
        self.closing.store(true, Ordering::Release);
    }

    pub fn bind(&self, generation: u64, sender: Sender) -> bool {
        {
            let Ok(mut connection) = self.connection.lock() else {
                return false;
            };
            self.offline_epoch.fetch_add(1, Ordering::AcqRel);
            self.processes.mark_online();
            *connection = Some((generation, sender));
        }
        for id in self.receipts.pending() {
            self.notify_receipt(&id);
        }
        true
    }

    pub fn unbind(self: &Arc<Self>, generation: u64) {
        self.unbind_with_grace(generation, OFFLINE_GRACE);
    }

    fn unbind_with_grace(self: &Arc<Self>, generation: u64, grace: Duration) {
        if let Ok(mut connection) = self.connection.lock()
            && connection
                .as_ref()
                .is_some_and(|(active, _)| *active == generation)
        {
            *connection = None;
            self.processes.mark_offline();
            let epoch = self.offline_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            let runtime = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(grace).await;
                if let Some(runtime) = runtime.upgrade() {
                    runtime.expire_offline(epoch);
                }
            });
        }
    }

    fn expire_offline(&self, epoch: u64) {
        if let Ok(connection) = self.connection.lock()
            && connection.is_none()
            && self.offline_epoch.load(Ordering::Acquire) == epoch
        {
            self.processes.stop_offline_jobs();
        }
    }

    pub fn acknowledge(&self, value: &Value) {
        self.receipts.acknowledge(
            value.get("operation_id").unwrap_or(&NULL),
            value.get("instance_id").unwrap_or(&NULL),
        );
    }

    fn notify_receipt(&self, id: &str) {
        let sender = self
            .connection
            .lock()
            .ok()
            .and_then(|connection| connection.as_ref().map(|(_, sender)| sender.clone()));
        let Some(sender) = sender else {
            return;
        };
        if let Ok(mut status) = self
            .receipts
            .status(&json!(id), &json!(self.receipts.instance_id))
        {
            status["type"] = json!("operation_result");
            let _ = sender.json(&status);
        }
    }

    pub fn info(&self) -> Value {
        let system = system::info();
        let mut operations = vec!["device_info", "get_operation_status"];
        operations.extend_from_slice(FILE_OPERATIONS);
        operations.extend_from_slice(SEARCH_OPERATIONS);
        operations.extend_from_slice(PROCESS_OPERATIONS);
        if self.startup.is_some() {
            operations.extend_from_slice(STARTUP_OPERATIONS);
        }
        let effective = self.policy.effective_operations(&operations, &self.config);
        let readonly: Vec<_> = self
            .config
            .roots
            .keys()
            .filter(|root| !self.config.allow_write || self.policy.readonly_roots.contains(*root))
            .cloned()
            .collect();
        let protected = self.policy.protected_paths();
        let mut limited = Vec::new();
        let mut bytes = 0;
        for path in &protected {
            bytes += serde_json::to_vec(path).map_or(0, |value| value.len()) + 1;
            if bytes > 2048 {
                break;
            }
            limited.push(path.clone());
        }
        let mut inherit_env = self.policy.inherit_env.clone();
        inherit_env.sort();
        let sandbox = self.sandbox.report();
        let (expose, hidden_paths, hidden_files) = self.sandbox.identity_paths();
        let enabled = self.audit.is_some();
        let required = self.audit.as_ref().is_some_and(|audit| audit.required());
        let audit_settings = json!({"enabled":enabled,"required":required});
        let identity = json!({"effective_operations":effective, "readonly_roots":readonly,
            "protected_paths":protected, "sandbox":sandbox, "inherit_env":inherit_env,
            "local_audit":audit_settings, "roots":self.config.roots, "expose":expose, "hiddenPaths":hidden_paths, "hiddenFiles":hidden_files});
        let revision = sha256_hex(&serde_json::to_vec(&identity).expect("policy identity is JSON"));
        let mut audit_health = audit_settings;
        if let Some(audit) = &self.audit
            && let (Some(target), Some(stats)) =
                (audit_health.as_object_mut(), audit.stats().as_object())
        {
            target.extend(stats.clone());
        }
        let hardening = json!({"version":1, "effective_operations":effective, "readonly_roots":readonly,
            "protected_paths":limited, "protected_paths_count":protected.len(),
            "protected_paths_truncated":limited.len() != protected.len(), "sandbox":sandbox,
            "inherit_env":inherit_env, "local_audit":audit_health, "policy_revision":revision});
        json!({"os":system.os, "release":system.release, "machine":system.machine, "hostname":system.hostname,
            "hardening":hardening, "roots":self.config.roots,
            "allow_shell":self.config.allow_shell && !PROCESS_OPERATIONS.is_empty(), "allow_write":self.config.allow_write,
            "max_processes":self.config.max_processes, "max_process_seconds":self.config.max_process_seconds,
            "shell":self.config.shell, "file_limit_bytes":1024 * 1024,
            "process_output_limit_chars":256 * 1024,
            "shell_authority":"Agent OS user; file roots do not sandbox shell commands",
            "capabilities":{"operation_receipts":{"version":1,"instance_id":self.receipts.instance_id,
                "max_records":self.receipts.max_records,"ttl_seconds":self.receipts.ttl.as_secs()},
                "request_budget":1,"chunk_encoding":1,"process_tree_termination":Processes::TREE_TERMINATION,
                "operations":operations},
            "search_limits":{"max_concurrent":2,"max_entries":1000,"max_depth":30,"max_visited":10000,
                "max_result_bytes":256 * 1024,"task_ttl_seconds":30},
            "offline_process_grace_seconds":300,
            "transport_limits":{"max_inflight_operations":ORDINARY_LIMIT,"max_response_bytes":MAX_FRAME,"max_buffered_bytes":MAX_QUEUED_BYTES},
            "chunk_file_limit_bytes":64 * 1024 * 1024})
    }

    fn audit(
        &self,
        request: &Request,
        status: &str,
        result: Option<&Value>,
        error: Option<&str>,
        started: Instant,
    ) -> std::result::Result<(), String> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        let record = AuditRecord {
            id: Some(request.id.clone()),
            operation: request.operation.clone(),
            status: status.into(),
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            params: request.params.clone(),
            result: result.cloned(),
            error: error.map(str::to_owned),
        };
        audit.record(record)
    }

    fn audit_diagnostic(
        self: &Arc<Self>,
        request: &Request,
        status: &str,
        result: Option<&Value>,
        error: Option<&str>,
        started: Instant,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let permit = match Arc::clone(&self.diagnostic_audit).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                audit.note_dropped("AUDIT_DIAGNOSTIC_BUSY");
                return;
            }
        };
        let record = AuditRecord {
            id: Some(request.id.clone()),
            operation: request.operation.clone(),
            status: status.into(),
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            params: request.params.clone(),
            result: result.cloned(),
            error: error.map(str::to_owned),
        };
        self.active.fetch_add(1, Ordering::AcqRel);
        let flight = DiagnosticFlight {
            runtime: Arc::clone(self),
            _permit: permit,
        };
        let audit = Arc::clone(audit);
        tokio::task::spawn_blocking(move || {
            let _ = audit.record(record);
            drop(flight);
        });
    }

    fn execute(self: &Arc<Self>, request: &Request, started: Instant) -> Outcome {
        let diagnostic = matches!(
            request.operation.as_str(),
            "device_info" | "get_operation_status"
        );
        if let Some(denial) = self.policy.denial(&request.operation, &request.params) {
            if self
                .audit(request, "denied", None, Some(&denial.message), started)
                .is_err()
            {
                return Outcome::Failure(Failure::new(
                    "audit_write_failed",
                    "Operation was rejected before execution; local audit failed",
                    "not_sent",
                ));
            }
            return Outcome::Failure(Failure::new(denial.code, denial.message, "not_sent"));
        }
        if self.closing.load(Ordering::Acquire) {
            return Outcome::Failure(Failure::new(
                "operation_busy",
                "Device is shutting down",
                "not_sent",
            ));
        }
        if Instant::now() >= request.deadline {
            return Outcome::Failure(Failure::new(
                "operation_timeout",
                "Request budget expired before execution",
                "not_sent",
            ));
        }
        if !diagnostic
            && self.audit.as_ref().is_some_and(|audit| audit.required())
            && self.audit(request, "started", None, None, started).is_err()
        {
            return Outcome::Failure(Failure::new(
                "audit_write_failed",
                "Operation was not executed because local audit failed",
                "not_sent",
            ));
        }
        if self.closing.load(Ordering::Acquire) {
            return Outcome::Failure(Failure::new(
                "operation_busy",
                "Device is shutting down",
                "not_sent",
            ));
        }
        if Instant::now() >= request.deadline {
            return Outcome::Failure(Failure::new(
                "operation_timeout",
                "Request budget expired before execution",
                "not_sent",
            ));
        }
        let result = match request.operation.as_str() {
            "device_info" => Ok(self.info()),
            "get_operation_status" => self
                .receipts
                .status(
                    request.params.get("operation_id").unwrap_or(&NULL),
                    request.params.get("instance_id").unwrap_or(&NULL),
                )
                .map_err(Failure::receipt),
            operation if STARTUP_OPERATIONS.contains(&operation) && self.startup.is_some() => {
                let startup = self.startup.as_ref().expect("checked above");
                let params = request
                    .params
                    .as_object()
                    .expect("request params are an object");
                if params
                    .keys()
                    .any(|key| operation == "get_startup" || key != "mode")
                {
                    Err(Failure::new(
                        "invalid_params",
                        "Unknown startup parameter",
                        "not_sent",
                    ))
                } else if operation == "get_startup" {
                    startup
                        .status()
                        .map_err(|error| Failure::new(error.code, error.message, error.state))
                } else {
                    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("");
                    Mode::parse(mode)
                        .and_then(|mode| startup.set(mode, false, request.deadline, &self.closing))
                        .map_err(|error| Failure::new(error.code, error.message, error.state))
                }
            }
            "start_search" if SEARCH_OPERATIONS.contains(&"start_search") => self
                .searches
                .start(&request.params, request.deadline, &self.closing)
                .map_err(|message| match message.as_str() {
                    START_EXPIRED => Failure::new("operation_timeout", message, "not_sent"),
                    SHUTTING_DOWN => Failure::new("operation_busy", message, "not_sent"),
                    _ => Failure::new("agent_error", message, "unknown"),
                }),
            "get_search_results" if SEARCH_OPERATIONS.contains(&"get_search_results") => self
                .searches
                .get(&request.params)
                .map_err(|message| Failure::new("agent_error", message, "unknown")),
            "stop_search" if SEARCH_OPERATIONS.contains(&"stop_search") => self
                .searches
                .stop(&request.params)
                .map_err(|message| Failure::new("agent_error", message, "unknown")),
            "list_searches" if SEARCH_OPERATIONS.contains(&"list_searches") => self
                .searches
                .list()
                .map_err(|message| Failure::new("agent_error", message, "unknown")),
            operation if PROCESS_OPERATIONS.contains(&operation) => self
                .processes
                .dispatch(
                    operation,
                    &request.params,
                    &request.id,
                    request.deadline,
                    &self.closing,
                )
                .map_err(|error| Failure {
                    code: error.code.into(),
                    message: error.message,
                    state: error.state,
                    summary: error.summary,
                }),
            operation if FILE_OPERATIONS.contains(&operation) => self
                .files
                .dispatch_controlled(operation, &request.params, request.deadline, &self.closing)
                .map_err(|message| match message.as_str() {
                    MUTATION_DEADLINE_EXPIRED => {
                        Failure::new("operation_timeout", message, "not_sent")
                    }
                    MUTATION_SHUTTING_DOWN => Failure::new("operation_busy", message, "not_sent"),
                    _ => Failure::new("agent_error", message, "unknown"),
                }),
            _ => Err(Failure::new("agent_error", "Unknown operation", "unknown")),
        };
        #[cfg(test)]
        if request.operation == "write_file" && result.is_ok() {
            let gate = self.completion_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.wait();
            }
        }
        if diagnostic {
            return match result {
                Ok(value) => {
                    self.audit_diagnostic(request, "ok", Some(&value), None, started);
                    Outcome::Success(value)
                }
                Err(error) => {
                    self.audit_diagnostic(request, "error", None, Some(&error.message), started);
                    Outcome::Failure(error)
                }
            };
        }
        match result {
            Ok(value) => {
                if self
                    .audit(request, "ok", Some(&value), None, started)
                    .is_err()
                {
                    let mut failure = Failure::new(
                        "audit_write_failed",
                        "Operation completed but its outcome could not be audited; inspect before retrying",
                        "succeeded",
                    );
                    failure.summary = result_summary(&value);
                    Outcome::Failure(failure)
                } else {
                    Outcome::Success(value)
                }
            }
            Err(error) => {
                if self
                    .audit(request, "error", None, Some(&error.message), started)
                    .is_err()
                {
                    Outcome::Failure(Failure::new(
                        "audit_write_failed",
                        "Operation failed and its outcome could not be audited; inspect before retrying",
                        "unknown",
                    ))
                } else {
                    Outcome::Failure(error)
                }
            }
        }
    }

    pub fn receive(
        self: &Arc<Self>,
        message: &Value,
        sender: &Sender,
        ids: &ActiveIds,
    ) -> std::result::Result<(), InvalidRequest> {
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.encode_utf16().count() <= 128)
            .ok_or(InvalidRequest)?;
        let operation = message
            .get("operation")
            .and_then(Value::as_str)
            .filter(|op| op.encode_utf16().count() <= 64)
            .ok_or(InvalidRequest)?;
        let params = message
            .get("params")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !params.is_object() {
            return Err(InvalidRequest);
        }
        if self.quarantined.load(Ordering::Acquire) {
            send_failure(
                sender,
                id,
                &Failure::new(
                    "operation_busy",
                    "Device is awaiting service update acceptance",
                    "not_sent",
                ),
                None,
            );
            return Ok(());
        }
        let budget = match message.get("execution_budget_ms") {
            None => RESPONSE_DEADLINE,
            Some(Value::Number(value)) => match value.as_f64().filter(|value| {
                value.is_finite()
                    && value.fract() == 0.0
                    && (1.0..=9_007_199_254_740_991.0).contains(value)
            }) {
                Some(value) => {
                    Duration::from_millis((value as u64).min(RESPONSE_DEADLINE.as_millis() as u64))
                }
                None => {
                    send_failure(
                        sender,
                        id,
                        &Failure::new(
                            "operation_timeout",
                            "Request budget expired before execution",
                            "not_sent",
                        ),
                        None,
                    );
                    return Ok(());
                }
            },
            _ => {
                send_failure(
                    sender,
                    id,
                    &Failure::new(
                        "operation_timeout",
                        "Request budget expired before execution",
                        "not_sent",
                    ),
                    None,
                );
                return Ok(());
            }
        };
        let deadline = Instant::now() + budget;
        let mut active = ids.lock().map_err(|_| InvalidRequest)?;
        if active.contains(id) {
            return Err(InvalidRequest);
        }
        if self.closing.load(Ordering::Acquire) {
            send_failure(
                sender,
                id,
                &Failure::new("operation_busy", "Device is shutting down", "not_sent"),
                None,
            );
            return Ok(());
        }
        let semaphore = if operation == "get_operation_status" {
            &self.status
        } else {
            &self.ordinary
        };
        let permit = match Arc::clone(semaphore).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                send_failure(
                    sender,
                    id,
                    &Failure::new("operation_busy", "Device busy", "not_sent"),
                    None,
                );
                return Ok(());
            }
        };
        let receipt_id = if let Some(receipt) = message.get("operation_receipt") {
            if !receipt.is_object() {
                send_failure(
                    sender,
                    id,
                    &Failure::new(
                        "operation_unavailable",
                        "Invalid operation receipt",
                        "not_sent",
                    ),
                    None,
                );
                return Ok(());
            }
            let receipt_id = receipt.get("operation_id").unwrap_or(&NULL);
            let instance = receipt.get("instance_id").unwrap_or(&NULL);
            match self
                .receipts
                .begin(receipt_id, instance, operation, &params)
            {
                Ok(Begin::New) => receipt_id.as_str().map(str::to_owned),
                Ok(Begin::Existing(status)) => {
                    let state = match status["execution_state"].as_str() {
                        Some("running") => "running",
                        Some("succeeded") => "succeeded",
                        Some("failed") => "failed",
                        Some("not_sent") => "not_sent",
                        _ => "unknown",
                    };
                    let mut failure = Failure::new(
                        "operation_unavailable",
                        "Operation already accepted; query its status",
                        state,
                    );
                    failure.summary = status["result_summary"].clone();
                    send_failure(sender, id, &failure, None);
                    return Ok(());
                }
                Err(error) => {
                    send_failure(sender, id, &Failure::receipt(error), None);
                    return Ok(());
                }
            }
        } else {
            None
        };
        active.insert(id.to_owned());
        drop(active);
        self.active.fetch_add(1, Ordering::AcqRel);
        let flight = Flight {
            runtime: Arc::clone(self),
            id: id.to_owned(),
            active_ids: Arc::clone(ids),
            _permit: permit,
        };
        let request = Request {
            id: id.to_owned(),
            operation: operation.to_owned(),
            params,
            deadline,
            receipt_id,
        };
        let response_sender = sender.clone();
        let response_id = id.to_owned();
        let (done, response) = oneshot::channel::<(Outcome, f64)>();
        self.responses.fetch_add(1, Ordering::AcqRel);
        let response_flight = ResponseFlight(Arc::clone(self));
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                flight.runtime.execute(&request, started)
            }))
            .unwrap_or_else(|_| {
                Outcome::Failure(Failure::new(
                    "agent_error",
                    "Operation failed unexpectedly; inspect before retrying",
                    "unknown",
                ))
            });
            if let Some(id) = &request.receipt_id {
                match &outcome {
                    Outcome::Success(value) => {
                        flight
                            .runtime
                            .receipts
                            .complete(id, "succeeded", value, None)
                    }
                    Outcome::Failure(error) => flight.runtime.receipts.complete(
                        id,
                        error.state,
                        &error.summary,
                        Some(&error.code),
                    ),
                }
                flight.runtime.notify_receipt(id);
            }
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            drop(flight);
            let _ = done.send((outcome, elapsed_ms));
        });
        tokio::spawn(async move {
            let _response_flight = response_flight;
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), response).await
            {
                Ok(Ok((Outcome::Success(value), elapsed))) => {
                    send_success(&response_sender, &response_id, value, elapsed)
                }
                Ok(Ok((Outcome::Failure(error), elapsed))) => {
                    send_failure(&response_sender, &response_id, &error, Some(elapsed))
                }
                Ok(Err(_)) => send_failure(
                    &response_sender,
                    &response_id,
                    &Failure::new(
                        "agent_error",
                        "Operation outcome is unknown; inspect before retrying",
                        "unknown",
                    ),
                    None,
                ),
                Err(_) => send_failure(
                    &response_sender,
                    &response_id,
                    &Failure::new(
                        "operation_timeout",
                        "Operation deadline exceeded; outcome may be unknown. Inspect before retrying.",
                        "unknown",
                    ),
                    None,
                ),
            }
        });
        Ok(())
    }

    pub async fn shutdown(&self) -> bool {
        self.closing.store(true, Ordering::Release);
        if let Ok(mut connection) = self.connection.lock() {
            *connection = None;
        }
        let wait = async {
            loop {
                let notified = self.finished.notified();
                if self.active.load(Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
        };
        let processes = Arc::clone(&self.processes);
        let process_shutdown =
            tokio::task::spawn_blocking(move || processes.close(SHUTDOWN_DEADLINE));
        let (searches_stopped, operations_stopped, processes_stopped) = tokio::join!(
            self.searches.close(SHUTDOWN_DEADLINE),
            tokio::time::timeout(SHUTDOWN_DEADLINE, wait),
            tokio::time::timeout(SHUTDOWN_DEADLINE, process_shutdown),
        );
        searches_stopped && operations_stopped.is_ok() && matches!(processes_stopped, Ok(Ok(true)))
    }
}

fn clipped(value: &str, limit: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .take_while(|character| {
            used += character.len_utf16();
            used <= limit
        })
        .collect()
}

fn send_success(sender: &Sender, id: &str, result: Value, elapsed: f64) {
    let value = json!({"type":"result", "id":id, "result":result, "execution_ms":elapsed});
    if serde_json::to_vec(&value).map_or(true, |value| value.len() > MAX_FRAME) {
        send_failure(
            sender,
            id,
            &Failure::new(
                "result_too_large",
                "Result exceeds the transport limit",
                "succeeded",
            ),
            None,
        );
    } else {
        let _ = sender.json(&value);
    }
}

fn send_failure(sender: &Sender, id: &str, failure: &Failure, elapsed: Option<f64>) {
    let mut value = json!({"type":"result", "id":id, "error":clipped(&failure.message, 1000),
        "error_code":failure.code, "execution_state":failure.state, "result_summary":failure.summary});
    if let Some(elapsed) = elapsed {
        value["execution_ms"] = json!(elapsed);
    }
    let _ = sender.json(&value);
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        all(
            target_os = "macos",
            feature = "experimental-macos-files",
            feature = "experimental-macos-processes",
            feature = "experimental-macos-audit"
        )
    )
))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    use crate::transport::{Frame, channel};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh-rpc-{}-{}",
                std::process::id(),
                crate::random::hex_token::<8>().unwrap()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn runtime(&self, audit: Option<Arc<AuditLog>>) -> Arc<Runtime> {
            let config = DeviceConfig::parse(json!({
                "gateway_url":"http://localhost:80", "device_id":"fixture",
                "token":"t".repeat(32), "roots":{"work":self.0}, "allow_write":true, "allow_shell":true
            }))
            .unwrap();
            Runtime::new(config, Policy::default(), audit).unwrap()
        }

        fn audit(&self) -> Arc<AuditLog> {
            Arc::new(AuditLog::open(&self.0.join("audit"), "fixture", 4096, 2, true).unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Release a blocked worker even when an assertion fails.
    struct GateGuard(Arc<CompletionGate>);

    impl Drop for GateGuard {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn ids() -> ActiveIds {
        Arc::new(Mutex::new(HashSet::new()))
    }

    fn write(id: &str, budget_ms: u64, receipt: Option<(&str, &str)>) -> Value {
        let mut request = json!({"type":"request", "id":id, "operation":"write_file",
            "execution_budget_ms":budget_ms,
            "params":{"root":"work", "path":"output", "content":"one effect"}});
        if let Some((operation_id, instance_id)) = receipt {
            request["operation_receipt"] =
                json!({"operation_id":operation_id, "instance_id":instance_id});
        }
        request
    }

    async fn next_frame(rx: &mut mpsc::Receiver<Frame>) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timed out waiting for frame")
            .expect("frame channel closed");
        let text = frame.message.into_text().unwrap();
        serde_json::from_str(&text).unwrap()
    }

    async fn result(rx: &mut mpsc::Receiver<Frame>, id: &str) -> Value {
        loop {
            let value = next_frame(rx).await;
            if value["type"] == "result" && value["id"] == id {
                return value;
            }
        }
    }

    async fn wait_finished(runtime: &Runtime) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = runtime.finished.notified();
                if runtime.active.load(Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("worker did not finish");
    }

    #[cfg(target_os = "linux")]
    async fn wait_responses(runtime: &Runtime) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while runtime.active.load(Ordering::Acquire) != 0
                || runtime.responses.load(Ordering::Acquire) != 0
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("responses did not settle");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_freeze_waits_for_result_delivery_and_receipt_acknowledgement() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime(None);
        let gate = Arc::new(CompletionGate::default());
        let _release_on_failure = GateGuard(gate.clone());
        *runtime.completion_gate.lock().unwrap() = Some(gate.clone());
        let (sender, mut rx, _) = channel();
        let active = ids();
        assert!(runtime.bind(1, sender.clone()));
        let operation_id = "b".repeat(32);
        runtime
            .receive(
                &write(
                    "write",
                    5000,
                    Some((&operation_id, &runtime.receipts.instance_id)),
                ),
                &sender,
                &active,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        assert!(runtime.try_freeze_idle().is_none());
        gate.release();
        wait_responses(&runtime).await;
        // Completed work still has outbound frames and an unacknowledged receipt.
        assert!(runtime.try_freeze_idle().is_none());
        let first = next_frame(&mut rx).await;
        let second = next_frame(&mut rx).await;
        assert!(
            [first["type"].as_str(), second["type"].as_str()].contains(&Some("operation_result"))
        );
        assert!(runtime.try_freeze_idle().is_none());
        runtime.acknowledge(
            &json!({"operation_id":operation_id,"instance_id":runtime.receipts.instance_id}),
        );
        let frozen = runtime
            .try_freeze_idle()
            .expect("completed work should be idle");
        let blocked = json!({"type":"request","id":"blocked","operation":"write_file",
            "params":{"root":"work","path":"after-freeze","content":"must not execute"}});
        runtime.receive(&blocked, &sender, &active).unwrap();
        assert_eq!(
            result(&mut rx, "blocked").await["error_code"],
            "operation_busy"
        );
        assert!(!fixture.0.join("after-freeze").exists());
        drop(frozen);
        runtime.receive(&blocked, &sender, &active).unwrap();
        assert!(result(&mut rx, "blocked").await["result"]["sha256"].is_string());
        assert!(fixture.0.join("after-freeze").exists());
        assert!(runtime.shutdown().await);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_freeze_refuses_to_abandon_a_queued_response_or_live_process() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime(None);
        let (sender, mut rx, _) = channel();
        let active = ids();
        assert!(runtime.bind(1, sender.clone()));
        runtime
            .receive(
                &json!({"type":"request","id":"info","operation":"device_info","params":{}}),
                &sender,
                &active,
            )
            .unwrap();
        wait_responses(&runtime).await;
        assert!(runtime.try_freeze_idle().is_none());
        let in_send = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        // Dequeued is not sent: the writer still owns the frame's byte budget.
        assert!(runtime.try_freeze_idle().is_none());
        let response: Value = serde_json::from_str(in_send.message.to_text().unwrap()).unwrap();
        assert!(response["result"]["capabilities"].is_object());
        drop(in_send);
        assert!(runtime.try_freeze_idle().is_some());

        runtime
            .receive(
                &json!({"type":"request","id":"start","operation":"start_process",
            "params":{"root":"work","command":"/bin/sleep 30","wait_seconds":0}}),
                &sender,
                &active,
            )
            .unwrap();
        let job = result(&mut rx, "start").await["result"]["job_id"].clone();
        wait_responses(&runtime).await;
        assert!(runtime.try_freeze_idle().is_none());
        runtime
            .receive(
                &json!({"type":"request","id":"stop","operation":"terminate_process",
            "params":{"job_id":job}}),
                &sender,
                &active,
            )
            .unwrap();
        assert_eq!(
            result(&mut rx, "stop").await["result"]["status"],
            "terminated"
        );
        wait_responses(&runtime).await;
        let frozen = runtime
            .try_freeze_idle()
            .expect("terminated process should be idle");
        drop(frozen);
        assert!(runtime.shutdown().await);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admission_reserves_status_slot_and_shutdown_cancels_queued_writes() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime(None);
        let files = Arc::clone(&runtime.files);
        let (locked, wait_locked) = oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _guard = files.hold_mutation_for_test();
            let _ = locked.send(());
            let _ = wait_release.recv();
        });
        wait_locked.await.unwrap();
        let (sender, mut rx, _) = channel();
        let ids = ids();
        for index in 0..ORDINARY_LIMIT {
            runtime
                .receive(&write(&format!("write-{index}"), 5000, None), &sender, &ids)
                .unwrap();
        }
        assert_eq!(runtime.active.load(Ordering::Acquire), ORDINARY_LIMIT);
        runtime
            .receive(&write("overflow", 5000, None), &sender, &ids)
            .unwrap();
        assert_eq!(
            result(&mut rx, "overflow").await["error_code"],
            "operation_busy"
        );
        let status = json!({"type":"request", "id":"status", "operation":"get_operation_status",
            "params":{"operation_id":"a".repeat(32), "instance_id":runtime.receipts.instance_id}});
        runtime.receive(&status, &sender, &ids).unwrap();
        assert_eq!(
            result(&mut rx, "status").await["result"]["execution_state"],
            "unknown"
        );
        assert!(runtime.shutdown().await);
        for _ in 0..ORDINARY_LIMIT {
            let frame = next_frame(&mut rx).await;
            assert_eq!(frame["error_code"], "operation_busy");
            assert_eq!(frame["execution_state"], "not_sent");
        }
        assert!(!fixture.0.join("output").exists());
        assert_eq!(runtime.ordinary.available_permits(), ORDINARY_LIMIT);
        release.send(()).unwrap();
        holder.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_effect_remains_owned_and_receipt_replays_after_reconnect() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime(None);
        let gate = Arc::new(CompletionGate::default());
        let _release_on_failure = GateGuard(gate.clone());
        *runtime.completion_gate.lock().unwrap() = Some(gate.clone());
        let (sender, mut rx, _) = channel();
        let ids = ids();
        runtime.bind(1, sender.clone());
        let operation_id = "a".repeat(32);
        let request = write(
            "write",
            200,
            Some((&operation_id, &runtime.receipts.instance_id)),
        );
        runtime.receive(&request, &sender, &ids).unwrap();
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(fixture.0.join("output")).unwrap(),
            b"one effect"
        );
        let timeout = result(&mut rx, "write").await;
        assert_eq!(timeout["error_code"], "operation_timeout");
        assert_eq!(timeout["execution_state"], "unknown");
        assert_eq!(runtime.ordinary.available_permits(), ORDINARY_LIMIT - 1);
        assert_eq!(
            runtime
                .receipts
                .status(&json!(operation_id), &json!(runtime.receipts.instance_id))
                .unwrap()["execution_state"],
            "running"
        );
        let mut duplicate = request.clone();
        duplicate["id"] = json!("duplicate-running");
        duplicate["execution_budget_ms"] = json!(1000);
        runtime.receive(&duplicate, &sender, &ids).unwrap();
        let already_running = result(&mut rx, "duplicate-running").await;
        assert_eq!(already_running["error_code"], "operation_unavailable");
        assert_eq!(already_running["execution_state"], "running");
        runtime.unbind(1);
        gate.release();
        wait_finished(&runtime).await;
        let (new_sender, mut new_rx, _) = channel();
        runtime.bind(2, new_sender.clone());
        let replay = next_frame(&mut new_rx).await;
        assert_eq!(replay["type"], "operation_result");
        assert_eq!(replay["operation_id"], operation_id);
        assert_eq!(replay["execution_state"], "succeeded");
        runtime.acknowledge(
            &json!({"operation_id":operation_id, "instance_id":runtime.receipts.instance_id}),
        );
        let (third_sender, mut third_rx, _) = channel();
        runtime.bind(3, third_sender.clone());
        assert!(third_rx.try_recv().is_err());
        duplicate["id"] = json!("duplicate-complete");
        runtime.receive(&duplicate, &third_sender, &ids).unwrap();
        let already_done = result(&mut third_rx, "duplicate-complete").await;
        assert_eq!(already_done["error_code"], "operation_unavailable");
        assert_eq!(already_done["execution_state"], "succeeded");
        assert_eq!(
            std::fs::read(fixture.0.join("output")).unwrap(),
            b"one effect"
        );
        assert!(runtime.shutdown().await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn required_audit_failure_distinguishes_before_and_after_effect() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime(Some(fixture.audit()));
        let (sender, mut rx, _) = channel();
        let active_ids = ids();
        let log = fixture.0.join("audit/fixture.jsonl");
        std::fs::rename(&log, fixture.0.join("audit/unavailable.jsonl")).unwrap();
        let operation_id = "b".repeat(32);
        runtime
            .receive(
                &write(
                    "before",
                    1000,
                    Some((&operation_id, &runtime.receipts.instance_id)),
                ),
                &sender,
                &active_ids,
            )
            .unwrap();
        let before = result(&mut rx, "before").await;
        assert_eq!(before["error_code"], "audit_write_failed");
        assert_eq!(before["execution_state"], "not_sent");
        assert!(!fixture.0.join("output").exists());
        assert!(runtime.shutdown().await);

        let fixture = Fixture::new();
        let runtime = fixture.runtime(Some(fixture.audit()));
        let gate = Arc::new(CompletionGate::default());
        let _release_on_failure = GateGuard(gate.clone());
        *runtime.completion_gate.lock().unwrap() = Some(gate.clone());
        let (sender, mut rx, _) = channel();
        let operation_id = "c".repeat(32);
        runtime
            .receive(
                &write(
                    "after",
                    2000,
                    Some((&operation_id, &runtime.receipts.instance_id)),
                ),
                &sender,
                &ids(),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(fixture.0.join("output")).unwrap(),
            b"one effect"
        );
        std::fs::rename(
            fixture.0.join("audit/fixture.jsonl"),
            fixture.0.join("audit/unavailable.jsonl"),
        )
        .unwrap();
        gate.release();
        let after = result(&mut rx, "after").await;
        assert_eq!(after["error_code"], "audit_write_failed");
        assert_eq!(after["execution_state"], "succeeded");
        let receipt = runtime
            .receipts
            .status(&json!(operation_id), &json!(runtime.receipts.instance_id))
            .unwrap();
        assert_eq!(receipt["execution_state"], "succeeded");
        assert_eq!(receipt["error_code"], "audit_write_failed");
        assert!(receipt["result_summary"]["sha256"].is_string());
        assert!(runtime.shutdown().await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn process_survives_reconnect_but_expires_on_the_original_offline_deadline() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime(Some(fixture.audit()));
        let (sender, mut rx, _) = channel();
        let ids = ids();
        runtime.bind(1, sender.clone());
        let request = json!({"type":"request", "id":"first-process", "operation":"start_process",
            "params":{"root":"work", "command":"/bin/sleep 10", "wait_seconds":0}});
        runtime.receive(&request, &sender, &ids).unwrap();
        let first = result(&mut rx, "first-process").await;
        let first_id = first["result"]["job_id"].as_str().unwrap().to_owned();
        assert_eq!(first["result"]["status"], "running");

        runtime.unbind_with_grace(1, Duration::from_millis(180));
        tokio::time::sleep(Duration::from_millis(40)).await;
        let (sender2, mut rx2, _) = channel();
        runtime.bind(2, sender2.clone());
        tokio::time::sleep(Duration::from_millis(190)).await;
        let read = || {
            runtime
                .processes
                .dispatch(
                    "read_process",
                    &json!({"job_id":first_id}),
                    "read",
                    Instant::now() + Duration::from_secs(1),
                    &runtime.closing,
                )
                .unwrap()
        };
        assert_eq!(
            read()["status"],
            "running",
            "an old offline timer stopped a reconnected job"
        );

        runtime.unbind_with_grace(2, Duration::from_millis(180));
        tokio::time::sleep(Duration::from_millis(80)).await;
        // A connection attempt that never became ready must not reset the timer.
        runtime.unbind_with_grace(3, Duration::from_secs(10));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = read();
                if state["status"] == "terminated" && state["cleanup_pending"] == false {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("offline process did not stop and clean up");

        runtime.bind(4, sender2.clone());
        let next = json!({"type":"request", "id":"second-process", "operation":"start_process",
            "params":{"root":"work", "command":"printf fresh", "wait_seconds":1}});
        runtime.receive(&next, &sender2, &ids).unwrap();
        let second = result(&mut rx2, "second-process").await;
        assert_eq!(second["result"]["status"], "exited");
        assert_eq!(second["result"]["output"], "fresh");
        let audit = std::fs::read_to_string(fixture.0.join("audit/fixture.jsonl")).unwrap();
        let entries: Vec<Value> = audit
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for id in ["first-process", "second-process"] {
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry["id"] == id && entry["op"] == "process_exit")
                    .count(),
                1
            );
        }
        assert!(!audit.contains("/bin/sleep 10") && !audit.contains("printf fresh"));
        assert!(runtime.shutdown().await);
    }
}
