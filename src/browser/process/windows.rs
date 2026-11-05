//! Restricted inherited CDP handles and a creation-time kill-on-close Job.
use crate::processes::{WindowsProcessAttributes, windows_pipe};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    future::Future,
    io::{self, Read, Write},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    pin::Pin,
    ptr::{null, null_mut},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use windows_sys::Win32::{
    Foundation::{
        FILETIME, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            EXTENDED_STARTUPINFO_PRESENT, GetProcessTimes, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, WaitForSingleObject,
        },
    },
};

pub(super) mod supervisor;

pub struct AsyncPipe {
    pipe: windows_pipe::Pipe,
    retry: Pin<Box<tokio::time::Sleep>>,
    waiting: bool,
}
impl AsyncPipe {
    fn new(pipe: windows_pipe::Pipe) -> Self {
        Self {
            pipe,
            retry: Box::pin(tokio::time::sleep(Duration::ZERO)),
            waiting: false,
        }
    }
    fn ready(&mut self, cx: &mut Context<'_>) -> bool {
        if !self.waiting {
            return true;
        }
        if self.retry.as_mut().poll(cx).is_pending() {
            return false;
        }
        self.waiting = false;
        true
    }
    fn delay(&mut self, cx: &mut Context<'_>) {
        self.waiting = true;
        self.retry
            .as_mut()
            .reset(tokio::time::Instant::now() + Duration::from_millis(5));
        let _ = self.retry.as_mut().poll(cx);
    }
}
impl AsyncRead for AsyncPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !self.ready(cx) {
            return Poll::Pending;
        }
        match self.pipe.read(buf.initialize_unfilled()) {
            Ok(count) => {
                buf.advance(count);
                Poll::Ready(Ok(()))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.delay(cx);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}
impl AsyncWrite for AsyncPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.ready(cx) {
            return Poll::Pending;
        }
        match self.pipe.write(buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.delay(cx);
                Poll::Pending
            }
            result => Poll::Ready(result),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub struct Child {
    job: OwnedHandle,
    process: OwnedHandle,
    thread: Option<OwnedHandle>,
    pid: u32,
    identity: String,
    supervisor: supervisor::Supervisor,
}
pub(super) fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut text: Vec<_> = value.encode_wide().collect();
    if text.contains(&0) {
        return Err(io::Error::other("NUL in native browser argument"));
    }
    text.push(0);
    Ok(text)
}

pub(super) fn creation_identity(process: HANDLE) -> io::Result<String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(format!(
        "{:08x}{:08x}",
        created.dwHighDateTime, created.dwLowDateTime
    ))
}

pub(super) struct NativeProcess {
    process: OwnedHandle,
    thread: OwnedHandle,
    pid: u32,
}

/// The only native process creator used by the browser and its completion witness.
pub(super) fn create_process(
    executable: &Path,
    args: &[OsString],
    profile: &Path,
    inherited: &[HANDLE],
    job: Option<HANDLE>,
    suspended: bool,
) -> io::Result<NativeProcess> {
    let mut command = vec![executable.as_os_str().to_owned()];
    command.extend_from_slice(args);
    let text = command
        .iter()
        .map(|arg| {
            arg.to_str()
                .map(crate::processes::windows_argument)
                .ok_or_else(|| io::Error::other("Invalid Unicode browser argument"))
        })
        .collect::<io::Result<Vec<_>>>()?
        .join(" ");
    let mut line = wide(OsStr::new(&text))?;
    let executable = wide(executable.as_os_str())?;
    let cwd = wide(profile.as_os_str())?;
    let env = environment(profile)?;
    let mut attributes = WindowsProcessAttributes::new()?;
    attributes.handles(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited)?;
    let jobs = job.map(|job| [job]);
    if let Some(jobs) = &jobs {
        attributes.handles(PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs)?;
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[0];
    startup.StartupInfo.hStdError = inherited[1];
    attributes.apply(&mut startup);
    let mut info = PROCESS_INFORMATION::default();
    let flags = CREATE_UNICODE_ENVIRONMENT
        | CREATE_NO_WINDOW
        | EXTENDED_STARTUPINFO_PRESENT
        | if suspended { CREATE_SUSPENDED } else { 0 };
    if unsafe {
        CreateProcessW(
            executable.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            flags,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut info,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(NativeProcess {
        process: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
        thread: unsafe { OwnedHandle::from_raw_handle(info.hThread) },
        pid: info.dwProcessId,
    })
}

pub(super) fn active_processes(job: HANDLE) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ActiveProcesses)
}
fn environment(profile: &Path) -> io::Result<Vec<u16>> {
    // Preserve only OS/locale prerequisites. Browser credentials, proxy settings,
    // arbitrary application variables and the parent PATH are not inherited.
    let mut values = std::collections::BTreeMap::<String, OsString>::new();
    for key in [
        "SystemRoot",
        "WINDIR",
        "SystemDrive",
        "COMSPEC",
        "LANG",
        "LC_ALL",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "USERNAME",
        "USERDOMAIN",
        "HOMEDRIVE",
        "HOMEPATH",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "PROGRAMDATA",
    ] {
        if let Some(value) = std::env::var_os(key) {
            values.insert(key.to_uppercase(), value);
        }
    }
    // Known-folder discovery needs the real OS profile variables. The explicit
    // user-data-dir isolates browser state; only scratch directories move here.
    for key in ["HOME", "TEMP", "TMP"] {
        values.insert(key.into(), profile.as_os_str().to_owned());
    }
    if let Some(system) = values.get("SYSTEMROOT") {
        values.insert(
            "PATH".into(),
            Path::new(system).join("System32").into_os_string(),
        );
    }
    let mut block = Vec::new();
    for (key, value) in values {
        let mut row = OsString::from(key);
        row.push("=");
        row.push(value);
        block.extend(wide(&row)?);
    }
    block.push(0);
    Ok(block)
}
impl Child {
    pub fn spawn(
        executable: &Path,
        profile: &Path,
        args: &[OsString],
    ) -> io::Result<(Self, AsyncPipe, AsyncPipe)> {
        let intent = supervisor::Supervisor::prepare(profile)?;
        let (input, child_input) = windows_pipe::pair(true)?;
        let (output, child_output) = windows_pipe::pair(false)?;
        let nul = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")?;
        let stderr = crate::storage::create_private_file(&profile.join("browser.stderr"))
            .map_err(|e| io::Error::other(e.to_string()))?;
        for handle in [nul.as_raw_handle(), stderr.as_raw_handle()] {
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        let inherited = [
            nul.as_raw_handle(),
            stderr.as_raw_handle(),
            child_input.as_raw_handle(),
            child_output.as_raw_handle(),
        ];
        let input_number =
            u32::try_from(child_input.as_raw_handle() as usize).map_err(io::Error::other)?;
        let output_number =
            u32::try_from(child_output.as_raw_handle() as usize).map_err(io::Error::other)?;
        let mut command = args.to_vec();
        command.push(format!("--remote-debugging-io-pipes={input_number},{output_number}").into());
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // The witness is running and has durably recorded ownership before any
        // browser can exist. It owns no CDP handle and cannot emit browser input.
        let supervisor =
            supervisor::Supervisor::spawn(&job, profile, inherited[0], inherited[1], intent)?;
        let native =
            match create_process(executable, &command, profile, &inherited, Some(raw), true) {
                Ok(native) => native,
                Err(error) => {
                    supervisor.finish_empty();
                    return Err(error);
                }
            };
        let mut child = Self {
            job,
            process: native.process,
            thread: Some(native.thread),
            pid: native.pid,
            identity: String::new(),
            supervisor,
        };
        child.identity = creation_identity(child.process.as_raw_handle()).unwrap_or_default();
        Ok((child, AsyncPipe::new(output), AsyncPipe::new(input)))
    }
    pub fn owner(&self) -> Value {
        json!({"version":2,"pid":self.pid,"start":self.identity,"container":"windows_job","retired":false,"supervisor_nonce":self.supervisor.nonce()})
    }
    pub fn start(&mut self) -> io::Result<()> {
        self.supervisor.arm()?;
        if let Some(thread) = self.thread.take()
            && unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub fn force_stop(&mut self) {
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
        }
        let _ = self.supervisor.arm();
    }
    pub fn stopped(&mut self) -> io::Result<bool> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(
                active_processes(self.job.as_raw_handle())? == 0 && self.supervisor.stopped()?
            ),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.force_stop();
    }
}
pub fn retired(path: &Path, owner: Option<&Value>) -> io::Result<bool> {
    supervisor::retired(path, owner)
}
pub fn detect(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_owned());
    }
    let mut candidates = Vec::new();
    for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(base) {
            for relative in [
                r"Google\Chrome\Application\chrome.exe",
                r"Microsoft\Edge\Application\msedge.exe",
                r"Chromium\Application\chrome.exe",
            ] {
                candidates.push(PathBuf::from(&base).join(relative));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}
