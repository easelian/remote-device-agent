//! A per-browser completion witness. It can stop a Job, never drive a page.
//!
//! The witness stays outside the browser Job and retains a query/terminate handle
//! across Agent death. Recovery accepts its durable empty-Job record, not a PID
//! disappearing or a named Job becoming inaccessible.
use super::{active_processes, create_process, creation_identity};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    ffi::OsString,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{DuplicateHandle, GetHandleInformation, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::TerminateJobObject,
        SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE},
        Threading::{
            CreateEventW, EVENT_MODIFY_STATE, GetCurrentProcess, GetProcessId,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SetEvent, TerminateProcess,
            WaitForMultipleObjects, WaitForSingleObject,
        },
    },
};

const FLAG: &str = "--internal-browser-supervisor";
const RECORD: &str = "supervisor.json";
const COMPLETED: &str = "completed.json";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    version: u8,
    nonce: String,
    parent_pid: u32,
    parent_started: String,
    pid: u32,
    started: String,
}

fn error() -> io::Error {
    io::Error::other("Browser process completion cannot be verified")
}
fn duplicate(handle: HANDLE, access: u32) -> io::Result<OwnedHandle> {
    let mut raw = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut raw,
            access,
            1,
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
fn event() -> io::Result<OwnedHandle> {
    let raw = unsafe { CreateEventW(null(), 1, 0, null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
fn signal(event: &OwnedHandle) -> io::Result<()> {
    if unsafe { SetEvent(event.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) struct Supervisor {
    process: OwnedHandle,
    armed: OwnedHandle,
    nonce: String,
}
impl Supervisor {
    pub fn prepare(profile: &Path) -> io::Result<Record> {
        let record = Record {
            version: 1,
            nonce: crate::random::hex_token::<16>().map_err(|_| error())?,
            parent_pid: std::process::id(),
            parent_started: crate::process_identity::current().map_err(|_| error())?,
            pid: 0,
            started: String::new(),
        };
        crate::storage::write_private_new(
            &profile.join(RECORD),
            &serde_json::to_vec(&record).map_err(|_| error())?,
        )
        .map_err(|_| error())?;
        Ok(record)
    }
    pub fn spawn(
        job: &OwnedHandle,
        profile: &Path,
        nul: HANDLE,
        stderr: HANDLE,
        record: Record,
    ) -> io::Result<Self> {
        let armed = event()?;
        let ready = event()?;
        let job = duplicate(job.as_raw_handle(), JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE)?;
        let parent = duplicate(
            unsafe { GetCurrentProcess() },
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
        )?;
        let child_armed = duplicate(armed.as_raw_handle(), PROCESS_SYNCHRONIZE)?;
        let child_ready = duplicate(ready.as_raw_handle(), EVENT_MODIFY_STATE)?;
        let nonce = record.nonce;
        let inherited = [
            nul,
            stderr,
            job.as_raw_handle(),
            parent.as_raw_handle(),
            child_armed.as_raw_handle(),
            child_ready.as_raw_handle(),
        ];
        let mut args = vec![FLAG.into()];
        for handle in &inherited[2..] {
            args.push((*handle as usize).to_string().into());
        }
        args.push(nonce.clone().into());
        args.push(profile.as_os_str().to_owned());
        let executable = std::env::current_exe()?;
        // In-process integration fixtures are test harnesses, not the rdh CLI.
        // A debug-only override supplies the separately built, hashed candidate.
        #[cfg(debug_assertions)]
        let executable = std::env::var_os("RDH_TEST_BROWSER_SUPERVISOR")
            .map(PathBuf::from)
            .unwrap_or(executable);
        if !executable.is_absolute() {
            return Err(error());
        }
        // Do not suspend the witness: parent death between CreateProcess and
        // ResumeThread must not strand an unstarted holder of the Job handle.
        let native = create_process(&executable, &args, profile, &inherited, None, false)?;
        let supervisor = Self {
            process: native.process,
            armed,
            nonce,
        };
        drop(native.thread);
        let handles = [ready.as_raw_handle(), supervisor.process.as_raw_handle()];
        if unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 5000) } != WAIT_OBJECT_0 {
            // No browser has been created yet, so no page or named profile can
            // survive this failed handshake. Bound the helper's own shutdown.
            supervisor.finish_empty();
            return Err(error());
        }
        Ok(supervisor)
    }
    pub fn nonce(&self) -> &str {
        &self.nonce
    }
    pub fn arm(&self) -> io::Result<()> {
        signal(&self.armed)
    }
    pub fn stopped(&self) -> io::Result<bool> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
    pub fn finish_empty(&self) {
        let _ = self.arm();
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 1000) } != WAIT_OBJECT_0 {
            unsafe {
                TerminateProcess(self.process.as_raw_handle(), 1);
                WaitForSingleObject(self.process.as_raw_handle(), 500);
            }
        }
    }
}

fn valid_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn read_record(path: &Path) -> io::Result<Record> {
    let bytes = crate::storage::read_private(path).map_err(|_| error())?;
    let record: Record = serde_json::from_slice(&bytes).map_err(|_| error())?;
    if record.version != 1
        || !((record.pid == 0 && record.started.is_empty())
            || (record.pid > 0 && valid_hex(&record.started, 16)))
        || record.parent_pid == 0
        || !valid_hex(&record.nonce, 32)
        || !valid_hex(&record.parent_started, 16)
    {
        return Err(error());
    }
    Ok(record)
}

pub fn retired(path: &Path, owner: Option<&Value>) -> io::Result<bool> {
    if owner.is_some_and(|owner| owner["container"] == "windows_job" && owner["retired"] == true) {
        return Ok(true);
    }
    // A previous candidate without a witness still has no completion proof.
    if owner.is_some_and(|owner| owner["container"] != "windows_job" || owner["version"] != 2) {
        return Ok(false);
    }
    let record = read_record(&path.join(RECORD))?;
    if record.pid == 0 {
        // No browser can be created before the witness publishes its identity
        // and acknowledges that write. A dead parent cannot advance this stage.
        return Ok(owner.is_none()
            && !crate::process_identity::running(record.parent_pid, &record.parent_started)
                .map_err(|_| error())?);
    }
    if owner.is_some_and(|owner| owner["supervisor_nonce"] != record.nonce) {
        return Err(error());
    }
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if path.join(COMPLETED).try_exists()? {
            let completed = read_record(&path.join(COMPLETED))?;
            if completed != record {
                return Err(error());
            }
            if !crate::process_identity::running(record.pid, &record.started)
                .map_err(|_| error())?
            {
                return Ok(true);
            }
        } else if crate::process_identity::running(record.parent_pid, &record.parent_started)
            .map_err(|_| error())?
        {
            return Ok(false);
        }
        if Instant::now() >= until {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Recognized before the public CLI parser, and only on native Windows builds.
pub fn run(args: &[OsString]) -> Option<io::Result<()>> {
    if args.first().is_none_or(|s| s != FLAG) {
        return None;
    }
    Some(run_inner(args))
}
fn run_inner(args: &[OsString]) -> io::Result<()> {
    if args.len() != 7 {
        return Err(error());
    }
    let mut raw_handles = Vec::new();
    for arg in &args[1..5] {
        let number = arg
            .to_str()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .ok_or_else(error)?;
        let handle = number as HANDLE;
        let mut flags = 0;
        if raw_handles.contains(&handle) || unsafe { GetHandleInformation(handle, &mut flags) } == 0
        {
            return Err(error());
        }
        raw_handles.push(handle);
    }
    // Every validated handle is inherited exclusively by this helper invocation.
    let handles: Vec<OwnedHandle> = raw_handles
        .into_iter()
        .map(|h| unsafe { OwnedHandle::from_raw_handle(h) })
        .collect();
    let (job, parent, armed, ready) = (&handles[0], &handles[1], &handles[2], &handles[3]);
    let nonce = args[5]
        .to_str()
        .filter(|s| valid_hex(s, 32))
        .ok_or_else(error)?;
    let path = PathBuf::from(&args[6]);
    if !path.is_absolute()
        || !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| valid_hex(s, 32))
        || !path
            .ancestors()
            .any(|p| p.file_name().is_some_and(|s| s == ".rdh-browser"))
    {
        return Err(error());
    }
    crate::storage::private_metadata(&path, true).map_err(|_| error())?;
    active_processes(job.as_raw_handle())?;
    let parent_pid = unsafe { GetProcessId(parent.as_raw_handle()) };
    if parent_pid == 0 || parent_pid == std::process::id() {
        return Err(error());
    }
    let intent = read_record(&path.join(RECORD))?;
    let record = Record {
        version: 1,
        nonce: nonce.into(),
        parent_pid,
        parent_started: creation_identity(parent.as_raw_handle())?,
        pid: std::process::id(),
        started: crate::process_identity::current().map_err(|_| error())?,
    };
    if intent.pid != 0
        || intent.nonce != record.nonce
        || intent.parent_pid != record.parent_pid
        || intent.parent_started != record.parent_started
    {
        return Err(error());
    }
    let bytes = serde_json::to_vec(&record).map_err(|_| error())?;
    crate::storage::write_private_replace(&path.join(RECORD), &bytes).map_err(|_| error())?;
    signal(ready)?;
    let waiting = [parent.as_raw_handle(), armed.as_raw_handle()];
    let first = unsafe { WaitForMultipleObjects(2, waiting.as_ptr(), 0, u32::MAX) };
    if first != WAIT_OBJECT_0 && first != WAIT_OBJECT_0 + 1 {
        return Err(error());
    }
    let mut stopped_parent = false;
    loop {
        let state = unsafe { WaitForSingleObject(parent.as_raw_handle(), 0) };
        if state == WAIT_OBJECT_0 && !stopped_parent {
            if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            stopped_parent = true;
        } else if state != WAIT_OBJECT_0 && state != WAIT_TIMEOUT {
            return Err(io::Error::last_os_error());
        }
        if active_processes(job.as_raw_handle())? == 0 {
            crate::storage::write_private_new(&path.join(COMPLETED), &bytes)
                .map_err(|_| error())?;
            return Ok(());
        }
        // Keep the Job handle until native termination completes, including slow
        // kernel I/O. A new Agent reports cleanup pending while this proof waits.
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (PathBuf, Record) {
        let path = std::env::temp_dir().join(format!(
            "rdh-browser-witness-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        crate::storage::create_private_dir(&path).unwrap();
        let record = Supervisor::prepare(&path).unwrap();
        (path, record)
    }
    fn save(path: &Path, name: &str, record: &Record) {
        crate::storage::write_private_replace(
            &path.join(name),
            &serde_json::to_vec(record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn recovery_requires_matching_completion_and_handles_pid_reuse() {
        let (path, mut record) = fixture();
        // A numeric PID reused by this process is not the old witness identity.
        record.pid = std::process::id();
        record.started = "0".repeat(16);
        save(&path, RECORD, &record);
        let owner = json!({"version":2,"container":"windows_job","supervisor_nonce":record.nonce});
        assert!(!retired(&path, Some(&owner)).unwrap());
        let mut different = record.clone();
        different.nonce = "f".repeat(32);
        save(&path, COMPLETED, &different);
        assert!(retired(&path, Some(&owner)).is_err());
        save(&path, COMPLETED, &record);
        assert!(retired(&path, Some(&owner)).unwrap());
        assert!(retired(&path, None).unwrap());
        let mut other = owner.clone();
        other["supervisor_nonce"] = "other".into();
        assert!(retired(&path, Some(&other)).is_err());
        assert!(
            !retired(
                &path,
                Some(&json!({"version":1,"container":"windows_job","retired":false}))
            )
            .unwrap()
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn pre_browser_intent_cannot_retire_a_live_parent_or_an_existing_browser() {
        let (path, mut record) = fixture();
        assert!(!retired(&path, None).unwrap());
        record.parent_started = "0".repeat(16);
        save(&path, RECORD, &record);
        assert!(retired(&path, None).unwrap());
        let owner = json!({"version":2,"container":"windows_job","supervisor_nonce":record.nonce});
        assert!(!retired(&path, Some(&owner)).unwrap());
        std::fs::remove_dir_all(path).unwrap();
    }
}
