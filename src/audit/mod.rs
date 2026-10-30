//! Private, bounded JSONL audit records. Required mode synchronizes each record
//! before reporting success. The caller must write `started` before effects.
//!
//! A mutex serializes this instance; the application's profile lock remains the
//! process-exclusion mechanism. This module does not protect against another
//! process running as the same user rewriting or truncating audit history.

use serde_json::Value;
use std::path::Path;

pub struct AuditRecord {
    pub id: Option<String>,
    pub operation: String,
    pub status: String,
    pub duration_ms: u64,
    pub params: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
}

pub struct AuditLog {
    required: bool,
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-audit"),
        all(windows, feature = "experimental-windows-audit")
    ))]
    state: std::sync::Mutex<implementation::State>,
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-audit"),
        all(windows, feature = "experimental-windows-audit")
    ))]
    health: std::sync::Arc<std::sync::Mutex<implementation::Health>>,
}

impl AuditLog {
    pub fn open(
        directory: &Path,
        name: &str,
        max_bytes: u64,
        max_files: usize,
        required: bool,
    ) -> Result<Self, String> {
        if !directory.is_absolute()
            || name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || !(4096..=9_007_199_254_740_991).contains(&max_bytes)
            || !(1..=20).contains(&max_files)
        {
            return Err("Invalid audit directory, name or limits".into());
        }
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        ))]
        {
            let health =
                std::sync::Arc::new(std::sync::Mutex::new(implementation::Health::default()));
            let state = implementation::State::open(
                directory,
                name,
                max_bytes,
                max_files,
                required,
                health.clone(),
            )
            .map_err(|error| format!("Local audit log is unavailable ({})", error.0))?;
            Ok(Self {
                required,
                state: std::sync::Mutex::new(state),
                health,
            })
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        )))]
        {
            let _ = required;
            Err("Private native audit storage is unsupported on this platform".into())
        }
    }

    /// Diagnostic callers can skip an occupied audit writer. Acquiring the
    /// mutex is nonblocking; once acquired, actual file I/O is still synchronous.
    pub fn try_record(&self, record: AuditRecord) -> Result<(), String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        ))]
        {
            match self.state.try_lock() {
                Ok(mut state) => state.record(record),
                Err(error) => {
                    let code = match error {
                        std::sync::TryLockError::WouldBlock => "AUDIT_BUSY",
                        std::sync::TryLockError::Poisoned(_) => "AUDIT_STATE_POISONED",
                    };
                    self.health
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .failed(code);
                    Err(format!("Local audit record was not written ({code})"))
                }
            }
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        )))]
        {
            let _ = record;
            Err("Private native audit storage is unsupported on this platform".into())
        }
    }

    pub fn record(&self, record: AuditRecord) -> Result<(), String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        ))]
        {
            match self.state.lock() {
                Ok(mut state) => state.record(record),
                Err(poisoned) => {
                    let mut state = poisoned.into_inner();
                    state.failed(implementation::Failure("AUDIT_STATE_POISONED".into()))
                }
            }
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        )))]
        {
            let _ = record;
            Err("Private native audit storage is unsupported on this platform".into())
        }
    }

    pub fn stats(&self) -> Value {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        ))]
        {
            // This short health lock is never held over file I/O or while
            // waiting for the main writer, so diagnostic queries stay usable.
            self.health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stats()
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        )))]
        {
            serde_json::json!({"written":0,"dropped":0,"last_error":"AUDIT_UNSUPPORTED_PLATFORM"})
        }
    }

    pub fn required(&self) -> bool {
        self.required
    }

    /// Record a skipped diagnostic audit without acquiring the writer or
    /// touching the filesystem. Accept only bounded diagnostic codes so caller
    /// error strings cannot expose secrets through the public health report.
    pub fn note_dropped(&self, message: &str) {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        ))]
        {
            let code = if !message.is_empty()
                && message.len() <= 64
                && message
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                message
            } else {
                "AUDIT_RECORD_DROPPED"
            };
            self.health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failed(code);
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-audit"),
            all(windows, feature = "experimental-windows-audit")
        )))]
        {
            let _ = message;
        }
    }
}

#[cfg(all(test, windows, not(feature = "experimental-windows-audit")))]
mod windows_default_tests {
    use super::AuditLog;

    #[test]
    fn audit_requires_explicit_experimental_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-audit-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        for required in [false, true] {
            assert!(AuditLog::open(&path, "device", 4096, 3, required).is_err());
            assert!(!path.exists());
        }
        std::fs::create_dir(&path).unwrap();
        let existing = path.join("device.jsonl");
        std::fs::write(&existing, b"original\n").unwrap();
        assert!(AuditLog::open(&path, "device", 4096, 3, true).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"original\n");
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(all(test, target_os = "macos", not(feature = "experimental-macos-audit")))]
mod macos_default_tests {
    use super::AuditLog;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn audit_requires_explicit_experimental_feature() {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rdh-default-audit-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        for required in [false, true] {
            assert!(AuditLog::open(&path, "device", 4096, 3, required).is_err());
            assert!(!path.exists());
        }
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let existing = path.join("device.jsonl");
        std::fs::write(&existing, b"original\n").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(AuditLog::open(&path, "device", 4096, 3, true).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"original\n");
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-audit")
))]
mod posix;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-audit"),
    all(windows, feature = "experimental-windows-audit")
))]
mod history;

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-audit"),
    all(windows, feature = "experimental-windows-audit")
))]
pub(crate) mod implementation {
    use super::*;
    use serde_json::{Map, json};
    use sha2::{Digest, Sha256};
    use std::{
        fs::File,
        io::{self, Write},
        path::Path,
    };

    #[cfg(unix)]
    use super::posix as platform;
    #[cfg(windows)]
    use crate::storage::windows::audit as platform;
    use platform::{Directory, Metadata, read_at, same_version};

    const MAX_RECORD: usize = 16 * 1024;
    const MAX_ENTRIES: usize = 1000;
    const ARCHIVES: usize = 20;

    #[derive(Debug)]
    pub(crate) struct Failure(pub(crate) String);
    pub(crate) type Result<T> = std::result::Result<T, Failure>;
    pub(crate) fn fail(code: &str) -> Failure {
        Failure(code.into())
    }
    #[cfg(unix)]
    impl From<io::Error> for Failure {
        fn from(error: io::Error) -> Self {
            let code = match error.raw_os_error() {
                Some(libc::EACCES) => "EACCES",
                Some(libc::EPERM) => "EPERM",
                Some(libc::ENOENT) => "ENOENT",
                Some(libc::ENOSPC) => "ENOSPC",
                Some(libc::EIO) => "EIO",
                Some(libc::EINVAL) => "EINVAL",
                Some(libc::EROFS) => "EROFS",
                Some(libc::ELOOP) => "ELOOP",
                Some(libc::ENOTDIR) => "ENOTDIR",
                Some(libc::EEXIST) => "EEXIST",
                Some(libc::EMFILE) => "EMFILE",
                Some(libc::ENFILE) => "ENFILE",
                Some(libc::EFBIG) => "EFBIG",
                Some(libc::ESTALE) => "ESTALE",
                _ => "ERROR",
            };
            fail(code)
        }
    }
    pub(crate) fn synchronize(file: &File) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        loop {
            use std::os::fd::AsRawFd;
            // Match the legacy Agent's fsync semantics. Rust's sync_all uses
            // F_FULLFSYNC on Apple platforms, including directory handles;
            // this backend explicitly checks POSIX fsync for both kinds.
            // SAFETY: file owns a live descriptor and fsync borrows it only.
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        #[cfg(not(target_os = "macos"))]
        file.sync_all()
    }
    fn clip(value: &str, units: usize) -> String {
        let mut used = 0;
        for (index, character) in value.char_indices() {
            used += character.len_utf16();
            if used > units {
                return format!("{}…", &value[..index]);
            }
        }
        value.into()
    }
    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }
    fn digest(value: &str) -> Value {
        json!({"bytes":value.len(),"sha256":sha256(value.as_bytes())})
    }
    fn summarize_params(params: &Value) -> Value {
        let mut summary = Map::new();
        let Some(params) = params.as_object() else {
            return Value::Object(summary);
        };
        for (key, value) in params {
            if key == "command" && value.is_string() {
                let command = value.as_str().unwrap_or_default();
                summary.insert("command_bytes".into(), json!(command.len()));
                summary.insert("command_sha256".into(), json!(sha256(command.as_bytes())));
                summary.insert("command_chars".into(), json!(command.chars().count()));
            } else if matches!(
                key.as_str(),
                "content" | "old_text" | "new_text" | "text" | "query"
            ) && value.is_string()
            {
                summary.insert(key.clone(), digest(value.as_str().unwrap_or_default()));
            } else if value.is_boolean() || value.is_number() {
                summary.insert(key.clone(), value.clone());
            } else if matches!(
                key.as_str(),
                "root"
                    | "path"
                    | "cwd"
                    | "source"
                    | "destination"
                    | "destination_root"
                    | "destination_path"
                    | "job_id"
                    | "search_id"
                    | "expected_sha256"
            ) && value.is_string()
            {
                summary.insert(
                    key.clone(),
                    json!(clip(value.as_str().unwrap_or_default(), 512)),
                );
            } else if key == "paths" && value.is_array() {
                let values = value.as_array().unwrap();
                summary.insert(
                    key.clone(),
                    Value::Array(
                        values
                            .iter()
                            .take(16)
                            .map(|v| v.as_str().map_or(Value::Null, |s| json!(clip(s, 256))))
                            .collect(),
                    ),
                );
                if values.len() > 16 {
                    summary.insert("paths_total".into(), json!(values.len()));
                }
            } else if key == "files" && value.is_array() {
                let values = value.as_array().unwrap();
                summary.insert(
                    key.clone(),
                    Value::Array(
                        values
                            .iter()
                            .take(16)
                            .map(|v| {
                                json!({
                                    "root":v.get("root").and_then(Value::as_str).map(|s|clip(s,64)),
                                    "path":v.get("path").and_then(Value::as_str).map(|s|clip(s,256))
                                })
                            })
                            .collect(),
                    ),
                );
                if values.len() > 16 {
                    summary.insert("files_total".into(), json!(values.len()));
                }
            } else if let Some(value) = value.as_str() {
                summary.insert(key.clone(), json!({"chars":value.chars().count()}));
            } else {
                summary.insert(
                    key.clone(),
                    json!({"type":if value.is_array() {"array"} else {"object"}}),
                );
            }
        }
        Value::Object(summary)
    }
    fn summarize_result(result: &Value) -> Option<Value> {
        let fields = result.as_object()?;
        let mut summary = Map::new();
        for key in [
            "size_bytes",
            "sha256",
            "exit_code",
            "status",
            "job_id",
            "pid",
            "total",
            "truncated",
            "moved",
            "created",
            "sent",
            "stdin_closed",
            "total_bytes",
            "visited",
            "search_id",
        ] {
            if let Some(value) = fields.get(key) {
                if let Some(value) = value.as_str() {
                    summary.insert(key.into(), json!(clip(value, 128)));
                } else if value.is_number() || value.is_boolean() || value.is_null() {
                    summary.insert(key.into(), value.clone());
                }
            }
        }
        for key in ["entries", "matches", "files", "jobs", "searches"] {
            if let Some(values) = fields.get(key).and_then(Value::as_array) {
                summary.insert(format!("{key}_count"), json!(values.len()));
            }
        }
        (!summary.is_empty()).then_some(Value::Object(summary))
    }
    #[cfg(windows)]
    fn timestamp() -> Result<String> {
        use windows_sys::Win32::Foundation::SYSTEMTIME;
        use windows_sys::Win32::System::SystemInformation::GetSystemTime;
        let mut time = SYSTEMTIME::default();
        // SAFETY: time is writable storage for the complete SYSTEMTIME output.
        unsafe { GetSystemTime(&mut time) };
        if !(1..=9999).contains(&time.wYear) {
            return Err(fail("AUDIT_CLOCK_INVALID"));
        }
        Ok(format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            time.wYear,
            time.wMonth,
            time.wDay,
            time.wHour,
            time.wMinute,
            time.wSecond,
            time.wMilliseconds
        ))
    }

    #[cfg(windows)]
    impl From<io::Error> for Failure {
        fn from(error: io::Error) -> Self {
            // Only bounded diagnostic codes can enter health reports.
            let code = match error.kind() {
                io::ErrorKind::NotFound => "ENOENT",
                io::ErrorKind::AlreadyExists => "EEXIST",
                io::ErrorKind::PermissionDenied => "EACCES",
                io::ErrorKind::InvalidInput => "EINVAL",
                io::ErrorKind::StorageFull => "ENOSPC",
                _ => "AUDIT_IO_ERROR",
            };
            fail(code)
        }
    }

    #[cfg(unix)]
    fn timestamp() -> Result<String> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| fail("AUDIT_CLOCK_INVALID"))?;
        // Infer the platform's time_t from gmtime_r. This keeps the checked
        // conversion without naming libc's deprecated musl alias.
        let seconds = elapsed
            .as_secs()
            .try_into()
            .map_err(|_| fail("AUDIT_CLOCK_INVALID"))?;
        let mut time = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: seconds points to initialized time_t and time has the required
        // output size. gmtime_r writes only through this unique output pointer.
        if unsafe { libc::gmtime_r(&seconds, time.as_mut_ptr()) }.is_null() {
            return Err(fail("AUDIT_CLOCK_INVALID"));
        }
        // SAFETY: gmtime_r succeeded and initialized the output structure.
        let time = unsafe { time.assume_init() };
        let year = time.tm_year + 1900;
        if !(0..=9999).contains(&year) {
            return Err(fail("AUDIT_CLOCK_INVALID"));
        }
        Ok(format!(
            "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            time.tm_mon + 1,
            time.tm_mday,
            time.tm_hour,
            time.tm_min,
            time.tm_sec,
            elapsed.subsec_millis()
        ))
    }
    pub(crate) fn write_all(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            match writer.write(&bytes[offset..]) {
                Ok(0) => return Err(fail("AUDIT_WRITE_NO_PROGRESS")),
                Ok(written) if written <= bytes.len() - offset => offset += written,
                Ok(_) => return Err(fail("AUDIT_INVALID_WRITE_COUNT")),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    #[derive(Default)]
    #[cfg(test)]
    struct Faults {
        append_after: Option<usize>,
        repair: bool,
        sync: bool,
        replace: bool,
        open: bool,
    }
    struct Rollback {
        size: u64,
        identity: platform::Identity,
    }
    #[derive(Default)]
    pub(super) struct Health {
        written: u64,
        dropped: u64,
        last_error: Option<String>,
    }
    impl Health {
        pub(super) fn failed(&mut self, code: &str) {
            self.dropped = self.dropped.saturating_add(1);
            self.last_error = Some(code.into());
        }
        fn written(&mut self) {
            self.written = self.written.saturating_add(1);
            self.last_error = None;
        }
        pub(super) fn stats(&self) -> Value {
            json!({"written":self.written,"dropped":self.dropped,"last_error":self.last_error})
        }
    }
    pub(super) struct State {
        directory: Directory,
        name: String,
        max_bytes: u64,
        max_files: usize,
        required: bool,
        boot: String,
        sequence: u64,
        previous: String,
        active: Option<File>,
        size: u64,
        retained: usize,
        rollback: Option<Rollback>,
        needs_prune: bool,
        health: std::sync::Arc<std::sync::Mutex<Health>>,
        #[cfg(test)]
        faults: Faults,
    }
    impl State {
        pub(super) fn open(
            path: &Path,
            name: &str,
            max_bytes: u64,
            max_files: usize,
            required: bool,
            health: std::sync::Arc<std::sync::Mutex<Health>>,
        ) -> Result<Self> {
            let directory = Directory::open(path, true)?;
            let mut state = Self {
                directory,
                name: format!("{name}.jsonl"),
                max_bytes,
                max_files,
                required,
                boot: crate::random::hex_token::<4>()
                    .map_err(|_| fail("AUDIT_RANDOM_UNAVAILABLE"))?,
                sequence: 0,
                previous: "0".repeat(16),
                active: None,
                size: 0,
                retained: 0,
                rollback: None,
                needs_prune: true,
                health,
                #[cfg(test)]
                faults: Faults::default(),
            };
            state.ensure_active()?;
            state.prune(0)?;
            Ok(state)
        }
        fn verify_file(&self, name: &str, file: &File) -> Result<Metadata> {
            self.directory.verify_file(name, file)
        }
        fn open_named(&self, name: &str, create: bool) -> Result<File> {
            self.directory.open_named(name, create)
        }
        fn ensure_active(&mut self) -> Result<()> {
            if self.active.is_none() {
                #[cfg(test)]
                if self.faults.open {
                    return Err(fail("ENOSPC"));
                }
                let file = self.open_named(&self.name, true)?;
                if self.required {
                    self.directory.synchronize()?;
                }
                self.size = self.verify_file(&self.name, &file)?.len();
                self.active = Some(file);
            }
            Ok(())
        }
        fn serialize(&mut self, record: AuditRecord) -> Result<Vec<u8>> {
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| fail("AUDIT_SEQUENCE_EXHAUSTED"))?;
            let mut entry = json!({"ts":timestamp()?,"boot":self.boot,"seq":self.sequence,"prev":self.previous,
                "op":clip(&record.operation,64),"status":clip(&record.status,64),"ms":record.duration_ms,
                "params":summarize_params(&record.params)});
            if let Some(id) = record.id {
                entry["id"] = json!(clip(&id, 128));
            }
            if let Some(result) = record.result.as_ref().and_then(summarize_result) {
                entry["result"] = result;
            }
            // Error text can include command/output/input content. Unlike the
            // old logger, never copy error strings verbatim, even when short.
            if let Some(error) = record.error.filter(|error| !error.is_empty()) {
                entry["error"] = digest(&error);
            }
            let limit = self.max_bytes.min(MAX_RECORD as u64) as usize;
            for stage in 0..3 {
                let mut bytes =
                    serde_json::to_vec(&entry).map_err(|_| fail("AUDIT_SERIALIZE_FAILED"))?;
                bytes.push(b'\n');
                if bytes.len() <= limit {
                    return Ok(bytes);
                }
                if stage == 0 {
                    entry["params"] = json!({"truncated":true});
                } else if stage == 1 {
                    entry["result"] = json!({"truncated":true});
                }
            }
            Err(fail("AUDIT_RECORD_TOO_LARGE"))
        }
        fn repair(&mut self) -> Result<()> {
            let Some(rollback) = self.rollback.as_ref() else {
                return Ok(());
            };
            #[cfg(test)]
            if self.faults.repair {
                return Err(fail("EIO"));
            }
            let file = self
                .active
                .as_ref()
                .ok_or_else(|| fail("AUDIT_REPAIR_NO_HANDLE"))?;
            let metadata = self.verify_file(&self.name, file)?;
            if platform::identity(&metadata) != rollback.identity || metadata.len() < rollback.size
            {
                return Err(fail("AUDIT_REPAIR_FILE_CHANGED"));
            }
            file.set_len(rollback.size)?;
            if self.required {
                synchronize(file)?;
            }
            self.size = rollback.size;
            self.rollback = None;
            Ok(())
        }
        pub(super) fn record(&mut self, record: AuditRecord) -> std::result::Result<(), String> {
            let result = self.append(record);
            if let Err(error) = result {
                let _ = self.repair();
                return self.failed(error);
            }
            Ok(())
        }
        pub(super) fn failed(&mut self, error: Failure) -> std::result::Result<(), String> {
            self.health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failed(&error.0);
            if self.required {
                Err(format!("Required local audit write failed ({})", error.0))
            } else {
                Ok(())
            }
        }
        fn append(&mut self, record: AuditRecord) -> Result<()> {
            self.repair()?;
            self.ensure_active()?;
            if self.needs_prune {
                self.prune(0)?;
            }
            let bytes = self.serialize(record)?;
            let metadata = self.verify_file(
                &self.name,
                self.active
                    .as_ref()
                    .ok_or_else(|| fail("AUDIT_NO_HANDLE"))?,
            )?;
            if metadata.len() != self.size {
                return Err(fail("AUDIT_SIZE_CHANGED"));
            }
            let rotated = self.size.saturating_add(bytes.len() as u64) > self.max_bytes;
            if rotated {
                self.rotate()?;
            }
            let pruned = self.retained >= MAX_ENTRIES;
            if pruned {
                self.prune(1)?;
            }
            let file = self
                .active
                .as_ref()
                .ok_or_else(|| fail("AUDIT_NO_HANDLE"))?;
            // Without retention work, no I/O or lock release separates this
            // append from the verified snapshot above. Recheck after any
            // rotation/compaction, which may have replaced the active file.
            let metadata = if rotated || pruned {
                self.verify_file(&self.name, file)?
            } else {
                metadata
            };
            if metadata.len() != self.size {
                return Err(fail("AUDIT_SIZE_CHANGED"));
            }
            self.rollback = Some(Rollback {
                size: self.size,
                identity: platform::identity(&metadata),
            });
            platform::prepare_append(file)?;
            #[cfg(test)]
            if let Some(count) = self.faults.append_after {
                write_all(&mut &*file, &bytes[..count.min(bytes.len())])?;
                return Err(fail("ENOSPC"));
            }
            write_all(&mut &*file, &bytes)?;
            if self.required {
                #[cfg(test)]
                if self.faults.sync {
                    return Err(fail("EIO"));
                }
                synchronize(file)?;
            }
            let after = self.verify_file(&self.name, file)?;
            if after.len() != self.size + bytes.len() as u64 {
                return Err(fail("AUDIT_SIZE_CHANGED"));
            }
            self.rollback = None;
            self.size = after.len();
            self.previous = sha256(&bytes)[..16].into();
            self.health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .written();
            self.retained += 1;
            Ok(())
        }
        fn archive(&self, index: usize) -> String {
            if index == 0 {
                self.name.clone()
            } else {
                format!("{}.{index}", self.name)
            }
        }
        fn tail(&self, name: &str, remaining: usize) -> Result<(Vec<u8>, usize, Metadata)> {
            let file = self.open_named(name, false)?;
            let before = self.verify_file(name, &file)?;
            if remaining == 0 {
                return Ok((Vec::new(), 0, before));
            }
            let length = before.len().min(((remaining + 1) * MAX_RECORD) as u64) as usize;
            let start = before.len() - length as u64;
            let mut bytes = vec![0; length];
            let mut offset = 0;
            while offset < length {
                let read = match read_at(&file, &mut bytes[offset..], start + offset as u64) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    other => other?,
                };
                if read == 0 {
                    return Err(fail("AUDIT_HISTORY_CHANGED"));
                }
                offset += read;
            }
            let after = self.verify_file(name, &file)?;
            if !same_version(&before, &after) {
                return Err(fail("AUDIT_HISTORY_CHANGED"));
            }
            let first = if start == 0 {
                0
            } else {
                bytes
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(length, |i| i + 1)
            };
            let end = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |i| i + 1);
            let mut lines = Vec::new();
            if end > first {
                for line in bytes[first..end].split_inclusive(|byte| *byte == b'\n') {
                    if line == b"\n" {
                        continue;
                    }
                    if line.len() > MAX_RECORD {
                        return Err(fail("AUDIT_HISTORY_RECORD_TOO_LARGE"));
                    }
                    if !history::valid_object(line) {
                        return Err(fail("AUDIT_INVALID_HISTORY"));
                    }
                    lines.push(line);
                }
            }
            let mut size: u64 = lines.iter().map(|line| line.len() as u64).sum();
            let mut keep = 0;
            while lines.len() - keep > remaining || size > self.max_bytes {
                size -= lines[keep].len() as u64;
                keep += 1;
            }
            Ok((lines[keep..].concat(), lines.len() - keep, before))
        }
        fn unlink(&self, name: &str) -> Result<()> {
            self.directory.unlink(name, self.required)
        }
        fn rename(&self, source: &str, target: &str) -> Result<()> {
            self.directory.rename(source, target, self.required)
        }
        fn replace(&mut self, name: &str, bytes: &[u8], expected: &Metadata) -> Result<()> {
            #[cfg(test)]
            let fail_before_rename = self.faults.replace;
            #[cfg(not(test))]
            let fail_before_rename = false;
            let preparation =
                self.directory
                    .replace(name, bytes, expected, self.required, fail_before_rename);
            if name == self.name {
                self.active = None;
                let reopen = self.ensure_active();
                preparation.and(reopen)
            } else {
                preparation
            }
        }
        fn prune(&mut self, reserve: usize) -> Result<()> {
            self.needs_prune = true;
            let mut remaining = MAX_ENTRIES - reserve;
            let mut retained = 0;
            // Historical archives outside the current max_files setting remain
            // eligible, exactly as in the existing Agent's retention contract.
            for index in 0..ARCHIVES {
                let name = self.archive(index);
                let (bytes, count, before) = match self.tail(&name, remaining) {
                    Ok(tail) => tail,
                    Err(error) if error.0 == "ENOENT" => continue,
                    Err(error) => return Err(error),
                };
                if index != 0 && count == 0 {
                    self.unlink(&name)?;
                } else if bytes.len() as u64 != before.len() {
                    self.replace(&name, &bytes, &before)?;
                }
                remaining -= count;
                retained += count;
            }
            self.retained = retained;
            self.needs_prune = false;
            self.ensure_active()?;
            Ok(())
        }
        fn rotate(&mut self) -> Result<()> {
            self.needs_prune = true;
            self.active = None;
            let rotated = (|| -> Result<()> {
                if self.max_files == 1 {
                    self.unlink(&self.name)?;
                } else {
                    self.unlink(&self.archive(self.max_files - 1))?;
                    for index in (1..self.max_files - 1).rev() {
                        match self.rename(&self.archive(index), &self.archive(index + 1)) {
                            Err(error) if error.0 == "ENOENT" => {}
                            other => other?,
                        }
                    }
                    self.rename(&self.name, &self.archive(1))?;
                }
                Ok(())
            })();
            let reopened = self.ensure_active();
            rotated.and(reopened)?;
            self.prune(0)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            fs::{self, OpenOptions},
            path::PathBuf,
            sync::Arc,
        };

        #[cfg(unix)]
        use std::{
            fs::DirBuilder,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
        };

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                    "rdh-rust-audit-{}-{}",
                    std::process::id(),
                    crate::random::hex_token::<12>().unwrap()
                ));
                #[cfg(unix)]
                DirBuilder::new().mode(0o700).create(&path).unwrap();
                #[cfg(windows)]
                platform::test_private_directory(&path);

                Self(path)
            }
            fn log(&self, bytes: u64, files: usize, required: bool) -> AuditLog {
                AuditLog::open(&self.0, "device", bytes, files, required).unwrap()
            }
            fn write(&self, name: &str, bytes: &[u8]) {
                #[cfg(unix)]
                {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(self.0.join(name))
                        .unwrap();
                    file.write_all(bytes).unwrap();
                }
                #[cfg(windows)]
                crate::storage::write_private_new(&self.0.join(name), bytes).unwrap();
            }
            fn raw(&self) -> Vec<u8> {
                fs::read(self.0.join("device.jsonl")).unwrap()
            }
            fn all(&self) -> Vec<Value> {
                let mut records = Vec::new();
                for index in (0..ARCHIVES).rev() {
                    let name = if index == 0 {
                        "device.jsonl".into()
                    } else {
                        format!("device.jsonl.{index}")
                    };
                    if let Ok(bytes) = fs::read(self.0.join(name)) {
                        records.extend(
                            bytes
                                .split(|b| *b == b'\n')
                                .filter(|line| !line.is_empty())
                                .map(|line| serde_json::from_slice::<Value>(line).unwrap()),
                        );
                    }
                }
                records
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fn entry(id: impl ToString) -> AuditRecord {
            AuditRecord {
                id: Some(id.to_string()),
                operation: "read_file".into(),
                status: "ok".into(),
                duration_ms: 12,
                params: json!({"path":"世界.txt"}),
                result: None,
                error: None,
            }
        }

        #[test]
        fn redacts_free_form_inputs_results_and_errors_with_compatible_metadata() {
            let fixture = Fixture::new();
            let log = fixture.log(5 * 1024 * 1024, 3, false);
            let command = "printf 😀 COMMAND_SECRET";
            let mut record = entry("request");
            record.params = json!({"command":command,"text":"STDIN_SECRET","content":"CONTENT_SECRET",
                "old_text":"OLD_SECRET","new_text":"NEW_SECRET","query":"QUERY_SECRET","custom":"CUSTOM_SECRET",
                "nested":{"value":"NESTED_SECRET"},"paths":["one","two"],"files":[{"root":"work","path":"three","content":"FILE_SECRET"}],"offset":3,"from_end":true});
            record.result = Some(
                json!({"content":"RESULT_SECRET","output":"OUTPUT_SECRET","exit_code":0,"jobs":[{"command":"JOB_SECRET"}]}),
            );
            record.error = Some("failed command COMMAND_SECRET STDIN_SECRET".into());
            log.record(record).unwrap();
            let raw = String::from_utf8(fixture.raw()).unwrap();
            assert!(!raw.contains("SECRET"));
            let value: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(value["params"]["command_bytes"], command.len());
            assert_eq!(value["params"]["command_chars"], command.chars().count());
            assert_eq!(
                value["params"]["command_sha256"],
                sha256(command.as_bytes())
            );
            assert_eq!(value["params"]["text"], digest("STDIN_SECRET"));
            assert_eq!(value["params"]["custom"], json!({"chars":13}));
            assert_eq!(value["result"], json!({"exit_code":0,"jobs_count":1}));
            assert!(value["error"]["sha256"].is_string());
            let timestamp = value["ts"].as_str().unwrap();
            assert_eq!(timestamp.len(), 24);
            assert_eq!(&timestamp[10..11], "T");
            assert_eq!(&timestamp[19..20], ".");
            assert!(timestamp.ends_with('Z'));
            assert_eq!(value["prev"], "0".repeat(16));
            assert_eq!(value["seq"], 1);
        }

        #[test]
        fn retains_latest_thousand_across_restart_and_preserves_hash_links() {
            let fixture = Fixture::new();
            let log = fixture.log(5 * 1024 * 1024, 3, false);
            for id in 0..1003 {
                log.record(entry(id)).unwrap();
            }
            let values = fixture.all();
            assert_eq!(values.len(), 1000);
            assert_eq!(values[0]["id"], "3");
            assert_eq!(values.last().unwrap()["id"], "1002");
            let raw = fixture.raw();
            let lines: Vec<&[u8]> = raw.split_inclusive(|b| *b == b'\n').collect();
            for pair in lines.windows(2) {
                let next: Value = serde_json::from_slice(pair[1]).unwrap();
                assert_eq!(next["prev"], sha256(pair[0])[..16]);
            }
            let boot = values.last().unwrap()["boot"].clone();
            assert_eq!(
                log.stats(),
                json!({"written":1003,"dropped":0,"last_error":null})
            );
            drop(log);
            let reopened = fixture.log(5 * 1024 * 1024, 3, false);
            reopened.record(entry(1003)).unwrap();
            let values = fixture.all();
            assert_eq!(values.len(), 1000);
            assert_eq!(values[0]["id"], "4");
            let last = values.last().unwrap();
            assert_ne!(last["boot"], boot);
            assert_eq!(last["seq"], 1);
            assert_eq!(last["prev"], "0".repeat(16));
        }

        #[test]
        fn startup_trims_partial_tail_and_legacy_archives_without_other_devices() {
            let fixture = Fixture::new();
            let lines: Vec<String> = (0..1100)
                .map(|id| format!("{{\"id\":\"{id}\"}}\n"))
                .collect();
            fixture.write(
                "device.jsonl",
                format!("{}{{\"incomplete\":", lines[600..].join("")).as_bytes(),
            );
            fixture.write("device.jsonl.1", lines[300..600].join("").as_bytes());
            fixture.write("device.jsonl.4", lines[..300].join("").as_bytes());
            fixture.write("device.jsonl.5", b"{\"id\":\"older\"}\n");
            fixture.write("other-device.jsonl", b"{\"keep\":true}\n");
            let log = fixture.log(5 * 1024 * 1024, 3, false);
            let values = fixture.all();
            assert_eq!(values.len(), 1000);
            assert_eq!(values[0]["id"], "100");
            assert!(fixture.0.join("device.jsonl.4").exists());
            assert!(!fixture.0.join("device.jsonl.5").exists());
            assert_eq!(
                fs::read(fixture.0.join("other-device.jsonl")).unwrap(),
                b"{\"keep\":true}\n"
            );
            assert!(
                !String::from_utf8(fixture.raw())
                    .unwrap()
                    .contains("incomplete")
            );
            log.record(entry("next")).unwrap();
            assert_eq!(fixture.all().len(), 1000);
            assert_eq!(fixture.all().last().unwrap()["id"], "next");
        }

        #[test]
        fn invalid_complete_history_is_rejected_without_rewriting_it() {
            for invalid in [
                b"{\"bad\":\"\xff\"}\n".as_slice(),
                b"{\"bad\":\"\\uD800\"}\n",
                b"{\"bad\":1e9999}\n",
                b"{\"bad\":true,}\n",
                b"[]\n",
            ] {
                let fixture = Fixture::new();
                fixture.write("device.jsonl", invalid);
                let result = AuditLog::open(&fixture.0, "device", 4096, 3, false);
                assert!(result.err().unwrap().contains("AUDIT_INVALID_HISTORY"));
                assert_eq!(fixture.raw(), invalid);
                assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
            }
        }

        #[test]
        fn rotation_and_unicode_records_respect_file_and_record_byte_budgets() {
            let fixture = Fixture::new();
            let log = fixture.log(4096, 3, false);
            for index in 0..30 {
                let mut record = entry(index);
                record.id = Some("界".repeat(128));
                record.params = json!({"paths":vec!["界".repeat(256);16]});
                record.result = Some(
                    json!({"sha256":"界".repeat(128),"job_id":"界".repeat(128),"status":"界".repeat(128)}),
                );
                log.record(record).unwrap();
            }
            let names: Vec<_> = fs::read_dir(&fixture.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert!(names.len() <= 3);
            assert!(names.len() > 1);
            for name in names {
                let bytes = fs::read(name).unwrap();
                assert!(bytes.len() <= 4096);
                for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                    assert!(line.len() <= 4096);
                    assert!(serde_json::from_slice::<Value>(line).unwrap().is_object());
                }
            }
            assert_eq!(log.stats()["written"], 30);
            assert_eq!(log.stats()["dropped"], 0);
            drop(log);
            let capped = Fixture::new();
            let log = capped.log(160 * 1024, 3, false);
            for id in 0..1003 {
                log.record(entry(id)).unwrap();
            }
            let values = capped.all();
            assert_eq!(values.len(), 1000);
            assert_eq!(values[0]["id"], "3");
        }

        #[test]
        fn required_audit_synchronizes_creation_rotation_and_reopen() {
            let fixture = Fixture::new();
            let log = fixture.log(4096, 3, true);
            for index in 0..30 {
                log.record(entry(index)).unwrap();
            }
            assert!(fixture.0.join("device.jsonl.1").is_file());
            assert_eq!(log.stats()["written"], 30);
            drop(log);
            let reopened = fixture.log(4096, 3, true);
            reopened.record(entry("reopened")).unwrap();
            assert_eq!(fixture.all().last().unwrap()["id"], "reopened");
            assert_eq!(
                reopened.stats(),
                json!({"written":1,"dropped":0,"last_error":null})
            );
        }

        #[test]
        fn failed_compaction_preserves_original_and_next_record_recovers() {
            let fixture = Fixture::new();
            let log = fixture.log(5 * 1024 * 1024, 3, false);
            for id in 0..1000 {
                log.record(entry(id)).unwrap();
            }
            let before = fixture.raw();
            log.state.lock().unwrap().faults.replace = true;
            log.record(entry("failed")).unwrap();
            assert_eq!(log.stats()["dropped"], 1);
            assert_eq!(fixture.raw(), before);
            assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
            log.state.lock().unwrap().faults.replace = false;
            log.record(entry("recovered")).unwrap();
            let values = fixture.all();
            assert_eq!(values.len(), 1000);
            assert_eq!(values[0]["id"], "1");
            assert_eq!(values.last().unwrap()["id"], "recovered");
            assert_eq!(
                log.stats(),
                json!({"written":1001,"dropped":1,"last_error":null})
            );
        }

        #[test]
        fn partial_write_and_failed_repair_block_new_appends_until_recovered() {
            let fixture = Fixture::new();
            let log = fixture.log(4096, 3, false);
            log.record(entry("before")).unwrap();
            let before = fixture.raw();
            {
                let mut state = log.state.lock().unwrap();
                state.faults.append_after = Some(20);
                state.faults.repair = true;
            }
            log.record(entry("incomplete")).unwrap();
            assert_eq!(fixture.raw().len(), before.len() + 20);
            assert_eq!(log.stats()["written"], 1);
            log.state.lock().unwrap().faults.append_after = None;
            log.record(entry("cannot-repair")).unwrap();
            assert_eq!(fixture.raw().len(), before.len() + 20);
            assert_eq!(log.stats()["dropped"], 2);
            log.state.lock().unwrap().faults.repair = false;
            log.record(entry("recovered")).unwrap();
            let values = fixture.all();
            assert_eq!(
                values
                    .iter()
                    .map(|v| v["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["before", "recovered"]
            );
            assert_eq!(values[1]["prev"], sha256(&before)[..16]);
            assert_eq!(
                log.stats(),
                json!({"written":2,"dropped":2,"last_error":null})
            );
        }

        #[test]
        fn required_sync_failure_rolls_back_and_reports_error_before_recovery() {
            let fixture = Fixture::new();
            let log = fixture.log(4096, 3, true);
            assert!(log.required());
            log.state.lock().unwrap().faults.sync = true;
            let mut started = entry("started");
            started.status = "started".into();
            assert!(
                log.record(started)
                    .unwrap_err()
                    .contains("Required local audit write failed")
            );
            assert!(fixture.raw().is_empty());
            assert_eq!(
                log.stats(),
                json!({"written":0,"dropped":1,"last_error":"EIO"})
            );
            log.state.lock().unwrap().faults.sync = false;
            log.record(entry("recovered")).unwrap();
            assert_eq!(fixture.all()[0]["seq"], 2);
            assert_eq!(fixture.all()[0]["prev"], "0".repeat(16));
            assert_eq!(
                log.stats(),
                json!({"written":1,"dropped":1,"last_error":null})
            );
        }

        #[test]
        fn rotation_open_failure_recovers_with_pending_retention() {
            let fixture = Fixture::new();
            let log = fixture.log(4096, 3, false);
            log.record(entry("before")).unwrap();
            {
                let mut state = log.state.lock().unwrap();
                state.active.as_ref().unwrap().set_len(4096).unwrap();
                state.size = 4096;
                state.faults.open = true;
            }
            log.record(entry("failed")).unwrap();
            assert_eq!(log.stats()["dropped"], 1);
            assert!(log.state.lock().unwrap().active.is_none());
            log.state.lock().unwrap().faults.open = false;
            log.record(entry("recovered")).unwrap();
            assert_eq!(
                fixture
                    .all()
                    .iter()
                    .map(|v| v["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["before", "recovered"]
            );
            assert_eq!(
                log.stats(),
                json!({"written":2,"dropped":1,"last_error":null})
            );
        }

        #[test]
        fn real_file_short_writes_retry_interrupts_and_zero_progress_fails() {
            struct Chunked<'a> {
                file: &'a File,
                interrupt: bool,
                zero: bool,
            }
            impl Write for Chunked<'_> {
                fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                    if self.interrupt {
                        self.interrupt = false;
                        return Err(io::Error::from(io::ErrorKind::Interrupted));
                    }
                    if self.zero {
                        return Ok(0);
                    }
                    (&*self.file).write(&bytes[..bytes.len().min(7)])
                }
                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }
            let fixture = Fixture::new();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(fixture.0.join("short")).unwrap();
            let bytes = "世界/😀\n".repeat(40).into_bytes();
            write_all(
                &mut Chunked {
                    file: &file,
                    interrupt: true,
                    zero: false,
                },
                &bytes,
            )
            .unwrap();
            assert_eq!(fs::read(fixture.0.join("short")).unwrap(), bytes);
            assert_eq!(
                write_all(
                    &mut Chunked {
                        file: &file,
                        interrupt: false,
                        zero: true
                    },
                    b"extra"
                )
                .unwrap_err()
                .0,
                "AUDIT_WRITE_NO_PROGRESS"
            );
            assert_eq!(fs::read(fixture.0.join("short")).unwrap(), bytes);
        }

        #[cfg(unix)]
        #[test]
        fn rejects_unsafe_permissions_symlinks_aliases_and_replaced_identity() {
            let unsafe_directory = Fixture::new();
            fs::set_permissions(&unsafe_directory.0, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(AuditLog::open(&unsafe_directory.0, "device", 4096, 3, false).is_err());
            let fixture = Fixture::new();
            fixture.write("target", b"keep");
            symlink("target", fixture.0.join("device.jsonl")).unwrap();
            assert!(AuditLog::open(&fixture.0, "device", 4096, 3, false).is_err());
            fs::remove_file(fixture.0.join("device.jsonl")).unwrap();
            fs::hard_link(fixture.0.join("target"), fixture.0.join("device.jsonl")).unwrap();
            assert!(AuditLog::open(&fixture.0, "device", 4096, 3, false).is_err());
            fs::remove_file(fixture.0.join("device.jsonl")).unwrap();
            let log = fixture.log(4096, 3, true);
            log.record(entry("before")).unwrap();
            fs::set_permissions(
                fixture.0.join("device.jsonl"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert!(log.record(entry("unsafe")).is_err());
            fs::set_permissions(
                fixture.0.join("device.jsonl"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            log.record(entry("recovered")).unwrap();
            fs::rename(fixture.0.join("device.jsonl"), fixture.0.join("saved")).unwrap();
            fixture.write("device.jsonl", b"");
            assert!(log.record(entry("replacement")).is_err());
            assert!(fixture.raw().is_empty());
            assert_eq!(fs::read(fixture.0.join("target")).unwrap(), b"keep");
        }

        #[test]
        fn shared_writer_has_one_chain_and_diagnostics_do_not_wait_for_main_lock() {
            let fixture = Fixture::new();
            let log = Arc::new(fixture.log(64 * 1024, 3, true));
            {
                let _writer = log.state.lock().unwrap();
                assert_eq!(log.stats()["written"], 0);
                assert!(
                    log.try_record(entry("busy"))
                        .unwrap_err()
                        .contains("AUDIT_BUSY")
                );
                assert_eq!(log.stats()["dropped"], 1);
                log.note_dropped("AUDIT_DIAGNOSTIC_BUSY");
                assert_eq!(log.stats()["dropped"], 2);
                assert_eq!(log.stats()["last_error"], "AUDIT_DIAGNOSTIC_BUSY");
            }
            std::thread::scope(|scope| {
                for worker in 0..4 {
                    let log = log.clone();
                    scope.spawn(move || {
                        for id in 0..10 {
                            log.record(entry(format!("{worker}-{id}"))).unwrap();
                        }
                    });
                }
            });
            let bytes = fixture.raw();
            let lines: Vec<&[u8]> = bytes.split_inclusive(|byte| *byte == b'\n').collect();
            assert_eq!(lines.len(), 40);
            for (index, line) in lines.iter().enumerate() {
                let record: Value = serde_json::from_slice(line).unwrap();
                assert_eq!(record["seq"], index + 1);
                if index > 0 {
                    assert_eq!(record["prev"], sha256(lines[index - 1])[..16]);
                }
            }
            assert_eq!(
                log.stats(),
                json!({"written":40,"dropped":2,"last_error":null})
            );
        }
    }
}
