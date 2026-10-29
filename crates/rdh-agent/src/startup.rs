//! Local approval and native registration for future Agent startup.
//! State changes are serialized with the legacy Agent's sidecar lock.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::profile_lock::ProfileLocks;
use crate::{random, storage};

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
))]
pub const STARTUP_OPERATIONS: &[&str] = &["get_startup", "set_startup"];
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-startup"),
    all(windows, feature = "experimental-windows-startup")
)))]
pub const STARTUP_OPERATIONS: &[&str] = &[];

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    None,
    User,
    Service,
}

impl Mode {
    pub fn parse(value: &str) -> StartupResult<Self> {
        match value {
            "none" => Ok(Self::None),
            "user" => Ok(Self::User),
            "service" => Ok(Self::Service),
            _ => Err(StartupFailure::new(
                "invalid_params",
                "mode must be none, user, or service",
                "not_sent",
            )),
        }
    }
}

#[derive(Debug)]
pub struct StartupFailure {
    pub code: &'static str,
    pub message: String,
    pub state: &'static str,
}
impl StartupFailure {
    fn new(code: &'static str, message: impl Into<String>, state: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            state,
        }
    }
    fn io(error: impl std::fmt::Display) -> Self {
        Self::new("agent_error", error.to_string(), "unknown")
    }
}
type StartupResult<T> = std::result::Result<T, StartupFailure>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Binding {
    pub mode: Mode,
    pub name: String,
}
#[derive(Clone, Deserialize, Serialize)]
struct Pending {
    request_id: String,
    mode: Mode,
    expires_at: u64,
}
#[derive(Deserialize, Serialize)]
struct State {
    version: u8,
    binding: Option<Binding>,
    pending: Option<Pending>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}
fn validate_binding(binding: &Binding) -> StartupResult<()> {
    if binding.mode == Mode::None || !valid_name(&binding.name) {
        Err(StartupFailure::new(
            "invalid_params",
            "Invalid startup binding",
            "not_sent",
        ))
    } else {
        Ok(())
    }
}
fn uuid() -> Result<String> {
    let mut value = random::bytes::<16>()?;
    value[6] = (value[6] & 0x0f) | 0x40;
    value[8] = (value[8] & 0x3f) | 0x80;
    let hex: String = value.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub(crate) trait StartupBackend: Send + Sync {
    fn enabled(&self, binding: &Binding) -> Result<bool>;
    fn set(&self, binding: &Binding, enabled: bool) -> Result<()>;
    fn create_user(&self) -> Result<Binding>;
    fn service_authority(&self) -> bool;
}
type Grant = dyn Fn(&Path, Mode) -> Result<bool> + Send + Sync;

pub struct Startup {
    config: PathBuf,
    file: PathBuf,
    backend: Arc<dyn StartupBackend>,
    granted: Arc<Grant>,
}

fn state_path(config: &Path) -> PathBuf {
    let mut name = config.as_os_str().to_os_string();
    name.push(".startup.json");
    PathBuf::from(name)
}

impl Startup {
    pub fn new(config: &Path) -> Result<Self> {
        let config = config.canonicalize()?;
        let backend = Arc::new(crate::startup_native::NativeStartup::new(config.clone())?);
        Ok(Self::with_backend(
            config,
            backend,
            Arc::new(crate::startup_grant::managed_grant),
        ))
    }
    fn with_backend(
        config: PathBuf,
        backend: Arc<dyn StartupBackend>,
        granted: Arc<Grant>,
    ) -> Self {
        Self {
            file: state_path(&config),
            config,
            backend,
            granted,
        }
    }
    fn read(&self) -> StartupResult<State> {
        Self::read_file(&self.file)
    }
    fn read_file(file: &Path) -> StartupResult<State> {
        let bytes = match storage::read_private(file) {
            Ok(value) => value,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(State {
                    version: 1,
                    binding: None,
                    pending: None,
                });
            }
            Err(error) => return Err(StartupFailure::io(error)),
        };
        if bytes.len() > 65536 {
            return Err(StartupFailure::io("Startup state is too large"));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(StartupFailure::io)?;
        let object = value
            .as_object()
            .ok_or_else(|| StartupFailure::io("Invalid startup state"))?;
        if object.get("version").and_then(Value::as_f64) != Some(1.0)
            || !object.contains_key("binding")
            || !object.contains_key("pending")
        {
            return Err(StartupFailure::io("Invalid startup state"));
        }
        let binding: Option<Binding> =
            serde_json::from_value(object["binding"].clone()).map_err(StartupFailure::io)?;
        let pending = if object["pending"].is_null() {
            None
        } else {
            let pending = object["pending"]
                .as_object()
                .ok_or_else(|| StartupFailure::io("Invalid startup approval request"))?;
            let request_id = pending
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| StartupFailure::io("Invalid startup approval request"))?;
            let mode = Mode::parse(pending.get("mode").and_then(Value::as_str).unwrap_or(""))?;
            let expires = pending
                .get("expires_at")
                .and_then(Value::as_f64)
                .filter(|value| {
                    value.is_finite()
                        && value.fract() == 0.0
                        && (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(value)
                })
                .ok_or_else(|| StartupFailure::io("Invalid startup approval request"))?;
            Some(Pending {
                request_id: request_id.into(),
                mode,
                expires_at: expires.max(0.0) as u64,
            })
        };
        let mut state = State {
            version: 1,
            binding,
            pending,
        };
        if let Some(binding) = &state.binding {
            validate_binding(binding)?;
        }
        if let Some(pending) = &state.pending {
            if pending.mode == Mode::None
                || pending.request_id.len() != 36
                || !pending.request_id.bytes().all(|byte| {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) || byte == b'-'
                })
                || pending.expires_at > 9_007_199_254_740_991
            {
                return Err(StartupFailure::io("Invalid startup approval request"));
            }
            if pending.expires_at <= now_ms() {
                state.pending = None;
            }
        }
        Ok(state)
    }
    fn write(&self, state: &State) -> StartupResult<()> {
        let mut data = serde_json::to_vec_pretty(state).map_err(StartupFailure::io)?;
        data.push(b'\n');
        storage::write_private_replace(&self.file, &data).map_err(StartupFailure::io)
    }
    fn status_for(&self, state: State) -> StartupResult<Value> {
        let mode = if let Some(binding) = &state.binding {
            if self.backend.enabled(binding).map_err(StartupFailure::io)? {
                binding.mode
            } else {
                Mode::None
            }
        } else {
            return Ok(
                json!({"mode":"unknown", "registration":null, "pending":state.pending,
                "current_connection":"unchanged", "authorization":"local_confirmation_or_administrator_grant",
                "shell_authority":"Commands running as the Agent OS user can change user startup independently"}),
            );
        };
        Ok(
            json!({"mode":mode, "registration":state.binding, "pending":state.pending,
            "current_connection":"unchanged", "authorization":"local_confirmation_or_administrator_grant",
            "shell_authority":"Commands running as the Agent OS user can change user startup independently"}),
        )
    }
    pub fn status(&self) -> StartupResult<Value> {
        self.status_for(self.read()?)
    }

    fn locked<T>(&self, action: impl FnOnce() -> StartupResult<T>) -> StartupResult<T> {
        let mut locks = ProfileLocks::acquire(std::slice::from_ref(&self.file)).map_err(|_| {
            StartupFailure::new(
                "startup_busy",
                "Startup settings are being changed or a stale startup lock needs local inspection",
                "not_sent",
            )
        })?;
        let result = action();
        locks.release().map_err(StartupFailure::io)?;
        result
    }
    fn check(deadline: Instant, closing: &AtomicBool) -> StartupResult<()> {
        if closing.load(Ordering::Acquire) {
            return Err(StartupFailure::new(
                "operation_busy",
                "Device is shutting down",
                "not_sent",
            ));
        }
        if Instant::now() >= deadline {
            return Err(StartupFailure::new(
                "operation_timeout",
                "Startup request expired before execution",
                "not_sent",
            ));
        }
        Ok(())
    }
    fn apply(
        &self,
        mut state: State,
        mode: Mode,
        deadline: Instant,
        closing: &AtomicBool,
        mut effect_started: bool,
    ) -> StartupResult<Value> {
        if mode != Mode::None
            && state
                .binding
                .as_ref()
                .is_some_and(|binding| binding.mode != mode)
        {
            return Err(StartupFailure::new(
                "startup_binding_conflict",
                "Changing service identity requires local administrator migration; the current connection is unchanged",
                "not_sent",
            ));
        }
        if state.binding.is_none() && mode == Mode::Service {
            return Err(StartupFailure::new(
                "startup_service_install_required",
                "Install and bind the advanced boot service locally before enabling it",
                "not_sent",
            ));
        }
        if state.binding.is_none() && mode == Mode::User {
            Self::check(deadline, closing)?;
            let binding = self.backend.create_user().map_err(StartupFailure::io)?;
            effect_started = true;
            validate_binding(&binding)
                .map_err(|error| Self::after_effect(error, effect_started))?;
            state.binding = Some(binding);
            self.write(&state)?; // Preserve the binding even if enabling fails.
        }
        if let Some(binding) = &state.binding {
            Self::check(deadline, closing)
                .map_err(|error| Self::after_effect(error, effect_started))?;
            self.backend.set(binding, mode != Mode::None).map_err(|_| StartupFailure::new(
                "startup_permission_required", "The operating system did not allow or confirm the startup change; inspect native state locally. No stop was requested.", "unknown"))?;
        }
        state.pending = None;
        self.write(&state)?;
        let mut result = self.status()?;
        result["status"] = json!(if state.binding.is_some() {
            "applied"
        } else {
            "unbound"
        });
        Ok(result)
    }

    fn after_effect(mut error: StartupFailure, effect_started: bool) -> StartupFailure {
        if effect_started && error.state == "not_sent" {
            error.state = "unknown";
            error
                .message
                .push_str("; a startup change already began, inspect its state before retrying");
        }
        error
    }

    pub fn set(
        &self,
        mode: Mode,
        local: bool,
        deadline: Instant,
        closing: &AtomicBool,
    ) -> StartupResult<Value> {
        self.locked(|| {
            let mut state = self.read()?;
            Self::check(deadline, closing)?;
            if mode == Mode::None {
                state.pending = None;
                self.write(&state)?; // Revocation survives a native disable failure.
                return self.apply(state, mode, deadline, closing, true);
            }
            if mode == Mode::Service && state.binding.as_ref().is_some_and(|binding| binding.mode == Mode::Service)
                && !self.backend.service_authority() {
                return Err(StartupFailure::new("startup_permission_required", "Use the local administrator command rdh startup system set service; this service account cannot change system startup", "not_sent"));
            }
            if local || (self.granted)(&self.config, mode).map_err(StartupFailure::io)? {
                return self.apply(state, mode, deadline, closing, false);
            }
            if state.pending.as_ref().is_some_and(|pending| pending.mode != mode) {
                return Err(StartupFailure::new("startup_busy", "A different startup request is awaiting local approval", "not_sent"));
            }
            if state.pending.is_none() {
                state.pending = Some(Pending { request_id: uuid().map_err(StartupFailure::io)?, mode,
                    expires_at: now_ms() + Duration::from_secs(600).as_millis() as u64 });
            }
            Self::check(deadline, closing)?;
            self.write(&state)?;
            let pending = state.pending.unwrap();
            Ok(json!({"status":"pending_local_approval", "request_id":pending.request_id,
                "mode":pending.mode,"expires_at":pending.expires_at,"current_connection":"unchanged"}))
        })
    }
    pub fn approve(
        &self,
        id: &str,
        accept: bool,
        deadline: Instant,
        closing: &AtomicBool,
    ) -> StartupResult<Value> {
        self.locked(|| {
            let mut state = self.read()?;
            Self::check(deadline, closing)?;
            let mode = state
                .pending
                .as_ref()
                .filter(|pending| pending.request_id == id)
                .map(|pending| pending.mode)
                .ok_or_else(|| {
                    StartupFailure::new(
                        "startup_request_expired",
                        "Approval request is missing, replaced, or expired",
                        "not_sent",
                    )
                })?;
            if !accept {
                state.pending = None;
                self.write(&state)?;
                let mut value = self.status()?;
                value["status"] = json!("denied");
                return Ok(value);
            }
            self.apply(state, mode, deadline, closing, false)
        })
    }
    pub fn bind(
        &self,
        mode: Mode,
        name: &str,
        deadline: Instant,
        closing: &AtomicBool,
    ) -> StartupResult<Value> {
        self.locked(|| {
            let mut state = self.read()?;
            let next = Binding {
                mode,
                name: name.into(),
            };
            validate_binding(&next)?;
            if state
                .binding
                .as_ref()
                .is_some_and(|binding| *binding != next)
            {
                return Err(StartupFailure::new(
                    "startup_binding_conflict",
                    "This profile is already bound to a different startup registration",
                    "not_sent",
                ));
            }
            self.backend.enabled(&next).map_err(StartupFailure::io)?;
            Self::check(deadline, closing)?;
            state.binding = Some(next);
            self.write(&state)?;
            self.status()
        })
    }
}

/// Read only the existing sidecar. This does not query or change a native
/// registration, and does not expose the approval request identifier.
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
))]
pub(crate) fn inspect_sidecar(config: &Path) -> Result<(Option<Binding>, Option<Mode>)> {
    let state = Startup::read_file(&state_path(config))
        .map_err(|_| Error::message("Startup state cannot be safely read"))?;
    Ok((state.binding, state.pending.map(|pending| pending.mode)))
}

#[cfg(all(windows, feature = "experimental-windows-install"))]
pub(crate) fn new_service_state(config: &Path, name: &str) -> Result<Vec<u8>> {
    let mut state = Startup::read_file(&state_path(config))
        .map_err(|_| Error::message("Startup state cannot be safely read"))?;
    if state.binding.is_some() {
        return Err(Error::message(
            "Service installation requires an unbound source profile; migrate an existing registration explicitly",
        ));
    }
    let binding = Binding {
        mode: Mode::Service,
        name: name.into(),
    };
    validate_binding(&binding).map_err(|_| Error::message("Invalid service registration name"))?;
    state.binding = Some(binding);
    // Preserve any approval request. Registering a stopped service does not
    // approve or enable a pending remote startup request.
    Ok(serde_json::to_vec(&state)?)
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
))]
pub(crate) fn sidecar_path(config: &Path) -> PathBuf {
    state_path(config)
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-startup"),
        all(windows, feature = "experimental-windows-startup")
    )
))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn request_deadline() -> Instant {
        // These cases assert approval/permission semantics, not filesystem
        // latency. Windows validates ACLs and publishes real private sidecars;
        // allow the local CLI's per-request budget on loaded native runners.
        // The explicitly expired request below still exercises deadline denial.
        Instant::now() + Duration::from_secs(30)
    }

    #[derive(Default)]
    struct Backend {
        enabled: AtomicBool,
        fail_disable: AtomicBool,
        authority: AtomicBool,
        calls: Mutex<Vec<String>>,
    }
    impl StartupBackend for Backend {
        fn enabled(&self, _: &Binding) -> Result<bool> {
            Ok(self.enabled.load(Ordering::Acquire))
        }
        fn set(&self, binding: &Binding, enabled: bool) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{:?}:{enabled}", binding.mode));
            if !enabled && self.fail_disable.load(Ordering::Acquire) {
                return Err(Error::message("fixture denial"));
            }
            self.enabled.store(enabled, Ordering::Release);
            Ok(())
        }
        fn create_user(&self) -> Result<Binding> {
            self.calls.lock().unwrap().push("create".into());
            Ok(Binding {
                mode: Mode::User,
                name: "rdh.service".into(),
            })
        }
        fn service_authority(&self) -> bool {
            self.authority.load(Ordering::Acquire)
        }
    }
    struct Fixture {
        path: PathBuf,
        startup: Arc<Startup>,
        backend: Arc<Backend>,
        granted: Arc<AtomicBool>,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("rdh-startup-{}", random::hex_token::<8>().unwrap()));
            #[cfg(unix)]
            std::fs::create_dir(&path).unwrap();
            #[cfg(windows)]
            {
                let (parents, name) =
                    crate::storage::windows::open_parent(&path, false, false).unwrap();
                drop(
                    crate::storage::windows::create_child(parents.last().unwrap(), &name, true)
                        .unwrap(),
                );
            }
            let backend = Arc::new(Backend::default());
            let granted = Arc::new(AtomicBool::new(false));
            let grant = Arc::clone(&granted);
            let startup = Arc::new(Startup::with_backend(
                path.join("agent.json"),
                backend.clone(),
                Arc::new(move |_, mode| Ok(mode == Mode::User && grant.load(Ordering::Acquire))),
            ));
            Self {
                path,
                startup,
                backend,
                granted,
            }
        }
        fn set(&self, mode: Mode, local: bool) -> StartupResult<Value> {
            self.startup
                .set(mode, local, request_deadline(), &AtomicBool::new(false))
        }
        fn approve(&self, id: &str, accept: bool) -> StartupResult<Value> {
            self.startup
                .approve(id, accept, request_deadline(), &AtomicBool::new(false))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn remote_enable_needs_exact_local_approval_and_disable_does_not_stop() {
        let fixture = Fixture::new();
        let request = fixture.set(Mode::User, false).unwrap();
        assert_eq!(request["status"], "pending_local_approval");
        assert!(fixture.backend.calls.lock().unwrap().is_empty());
        assert_eq!(
            fixture.set(Mode::User, false).unwrap()["request_id"],
            request["request_id"]
        );
        assert_eq!(
            fixture.set(Mode::Service, false).unwrap_err().code,
            "startup_busy"
        );
        assert_eq!(
            fixture.approve("different", true).unwrap_err().code,
            "startup_request_expired"
        );
        let id = request["request_id"].as_str().unwrap();
        assert_eq!(fixture.approve(id, true).unwrap()["mode"], "user");
        assert_eq!(
            fixture.approve(id, true).unwrap_err().code,
            "startup_request_expired"
        );
        assert_eq!(fixture.set(Mode::None, false).unwrap()["mode"], "none");
        assert_eq!(
            *fixture.backend.calls.lock().unwrap(),
            ["create", "User:true", "User:false"]
        );
    }

    #[test]
    fn grants_are_checked_per_request_and_denial_clears_pending_even_when_disable_fails() {
        let fixture = Fixture::new();
        fixture.granted.store(true, Ordering::Release);
        assert_eq!(fixture.set(Mode::User, false).unwrap()["mode"], "user");
        fixture.granted.store(false, Ordering::Release);
        let pending = fixture.set(Mode::User, false).unwrap();
        assert_eq!(pending["status"], "pending_local_approval");
        fixture.backend.fail_disable.store(true, Ordering::Release);
        assert_eq!(
            fixture.set(Mode::None, false).unwrap_err().code,
            "startup_permission_required"
        );
        assert!(fixture.startup.read().unwrap().pending.is_none());
        assert_eq!(
            fixture
                .approve(pending["request_id"].as_str().unwrap(), true)
                .unwrap_err()
                .code,
            "startup_request_expired"
        );
        assert_eq!(
            fixture.set(Mode::Service, true).unwrap_err().code,
            "startup_binding_conflict"
        );
    }

    #[test]
    fn legacy_sidecar_expiry_locks_and_deadlines_are_enforced() {
        let fixture = Fixture::new();
        let legacy = json!({"version":1.0,"binding":null,"pending":{
            "request_id":"01234567-89ab-4def-8123-456789abcdef", "mode":"user", "expires_at":(now_ms()+60000) as f64}});
        storage::write_private_replace(
            &fixture.startup.file,
            &serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let pending = fixture.startup.status().unwrap();
        assert_eq!(
            pending["pending"]["request_id"],
            legacy["pending"]["request_id"]
        );
        let mut lock = ProfileLocks::acquire(std::slice::from_ref(&fixture.startup.file)).unwrap();
        assert_eq!(
            fixture.set(Mode::None, false).unwrap_err().code,
            "startup_busy"
        );
        lock.release().unwrap();
        let mut state = fixture.startup.read().unwrap();
        state.pending.as_mut().unwrap().expires_at = now_ms().saturating_sub(1);
        fixture.startup.write(&state).unwrap();
        assert_eq!(
            fixture
                .approve("01234567-89ab-4def-8123-456789abcdef", true)
                .unwrap_err()
                .code,
            "startup_request_expired"
        );
        let result = fixture.startup.set(
            Mode::User,
            true,
            Instant::now() - Duration::from_secs(1),
            &AtomicBool::new(false),
        );
        assert_eq!(result.unwrap_err().code, "operation_timeout");
        assert!(fixture.backend.calls.lock().unwrap().is_empty());
        storage::write_private_replace(&fixture.startup.file, br#"{"version":1,"pending":null}"#)
            .unwrap();
        assert!(fixture.startup.status().is_err());
    }

    #[test]
    fn service_binding_needs_existing_registration_and_administrator_authority() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.set(Mode::Service, true).unwrap_err().code,
            "startup_service_install_required"
        );
        fixture
            .startup
            .bind(
                Mode::Service,
                "existing.service",
                request_deadline(),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            fixture.set(Mode::Service, false).unwrap_err().code,
            "startup_permission_required"
        );
        assert!(fixture.startup.read().unwrap().pending.is_none());
    }

    #[test]
    fn simultaneous_approval_and_cancellation_cannot_replay_a_cancelled_request() {
        let fixture = Fixture::new();
        let id = fixture.set(Mode::User, false).unwrap()["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let startup = fixture.startup.clone();
        let ready = barrier.clone();
        let cancel = std::thread::spawn(move || {
            ready.wait();
            startup.set(
                Mode::None,
                false,
                request_deadline(),
                &AtomicBool::new(false),
            )
        });
        let startup = fixture.startup.clone();
        let ready = barrier.clone();
        let request_id = id.clone();
        let approve = std::thread::spawn(move || {
            ready.wait();
            startup.approve(
                &request_id,
                true,
                request_deadline(),
                &AtomicBool::new(false),
            )
        });
        barrier.wait();
        let cancel = cancel.join().unwrap();
        let approve = approve.join().unwrap();
        if let Err(error) = cancel {
            assert_eq!(error.code, "startup_busy");
            assert!(approve.is_ok());
        } else {
            assert!(fixture.startup.read().unwrap().pending.is_none());
            assert!(!fixture.backend.enabled.load(Ordering::Acquire));
            assert_eq!(
                fixture.approve(&id, true).unwrap_err().code,
                "startup_request_expired"
            );
        }
    }
}
