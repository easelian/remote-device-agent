//! Windows children belong to a kill-on-close Job before any user code runs.
use super::*;
use crate::files::WindowsWorkingDirectory;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_BAD_EXE_FORMAT, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND,
        ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_STATE, ERROR_PATH_NOT_FOUND, HANDLE, WAIT_FAILED,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
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
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread,
            STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

#[path = "processes_windows_pipe.rs"]
mod pipe;
pub(super) type Input = pipe::Pipe;

type Environment = BTreeMap<String, (String, OsString)>;

pub(super) struct Plan {
    directory: WindowsWorkingDirectory,
    cwd: PathBuf,
    shell: String,
    command_line: Vec<u16>,
    environment: Environment,
    deadline: Instant,
}

pub(super) struct Child {
    // No job handle is inheritable. Closing the last owner also covers a
    // crashing parent before it can run the ordinary cleanup loop.
    job: OwnedHandle,
    process: OwnedHandle,
    thread: Option<OwnedHandle>,
    pid: u32,
    stdin: Option<Input>,
    stdout: Option<pipe::Pipe>,
    stderr: Option<pipe::Pipe>,
    directory: Option<WindowsWorkingDirectory>,
    deadline: Instant,
}

pub(super) fn cancelled_error() -> io::Error {
    io::Error::from_raw_os_error(ERROR_CANCELLED as i32)
}

pub(super) fn os_error(error: &io::Error, syscall: &str) -> Value {
    let errno = error.raw_os_error();
    let code = match errno.map(|code| code as u32) {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => "ENOENT",
        Some(ERROR_ACCESS_DENIED) => "EACCES",
        Some(ERROR_BAD_EXE_FORMAT) => "ENOEXEC",
        Some(ERROR_CANCELLED) => "ECANCELED",
        _ => "OS_ERROR",
    };
    json!({"name":"Error", "code":code, "errno":errno, "syscall":syscall})
}

fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut result: Vec<u16> = value.encode_wide().collect();
    if result.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Embedded NUL in process argument",
        ));
    }
    result.push(0);
    Ok(result)
}

// Windows CRT argument quoting, used by PowerShell's normal argv parser.
// CMD's /c payload is instead passed verbatim, matching the legacy launcher.
pub(crate) fn quote(argument: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in argument.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        result.push(ch);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

pub(super) fn prepare(
    inner: &Inner,
    root: &str,
    cwd: &str,
    command: &str,
    deadline: Instant,
) -> Result<Plan, ProcessFailure> {
    if inner.sandbox.is_isolated() {
        return Err(invalid(
            "Configured POSIX sandbox is unavailable on Windows",
        ));
    }
    let directory = inner
        .files
        .windows_working_directory(root, cwd)
        .map_err(invalid)?;
    let cwd = Path::new(
        inner
            .config
            .roots
            .get(root)
            .ok_or_else(|| invalid("Unknown root"))?,
    )
    .join(cwd);
    let shell = &inner.config.shell;
    let name = shell
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let powershell = matches!(
        name.as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    );
    if !powershell && !matches!(name.as_str(), "cmd" | "cmd.exe") {
        return Err(invalid(
            "Windows shell must be cmd.exe, powershell.exe or pwsh.exe",
        ));
    }
    let line = if powershell {
        format!(
            "{} -NoLogo -NoProfile -NonInteractive -Command {}",
            quote(shell),
            quote(command)
        )
    } else {
        format!("{} /d /s /c \"{}\"", quote(shell), command)
    };
    let command_line = wide(OsStr::new(&line)).map_err(|error| invalid(error.to_string()))?;
    if command_line.len() > 32767 {
        return Err(invalid("Windows command line exceeds 32767 UTF-16 units"));
    }
    Ok(Plan {
        directory,
        cwd,
        shell: shell.clone(),
        command_line,
        environment: environment(&inner.inherit_env),
        deadline,
    })
}

fn environment(inherit: &[String]) -> Environment {
    let source = std::env::vars_os()
        .map(|(name, value)| (name.to_string_lossy().to_ascii_uppercase(), value))
        .collect::<BTreeMap<_, _>>();
    environment_from(&source, inherit)
}

fn environment_from(source: &BTreeMap<String, OsString>, inherit: &[String]) -> Environment {
    let mut env = Environment::new();
    for name in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "TERM",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "SESSIONNAME",
        "ALLUSERSPROFILE",
        "PROGRAMDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
    ]
    .into_iter()
    .chain(inherit.iter().map(String::as_str))
    {
        if let Some(value) = source.get(&name.to_ascii_uppercase()) {
            env.entry(name.to_ascii_uppercase())
                .or_insert_with(|| (name.into(), value.clone()));
        }
    }
    let system_root = get_env(&env, "SYSTEMROOT")
        .filter(|value| !value.is_empty())
        .or_else(|| get_env(&env, "WINDIR").filter(|value| !value.is_empty()))
        .unwrap_or_else(|| OsStr::new(r"C:\Windows"))
        .to_os_string();
    fallback(&mut env, "SystemRoot", system_root.clone());
    fallback(&mut env, "WINDIR", system_root.clone());
    fallback(
        &mut env,
        "COMSPEC",
        Path::new(&system_root)
            .join("System32")
            .join("cmd.exe")
            .into_os_string(),
    );
    fallback(
        &mut env,
        "PATH",
        Path::new(&system_root).join("System32").into_os_string(),
    );
    let mut extensions = get_env(&env, "PATHEXT")
        .unwrap_or_default()
        .to_string_lossy()
        .split(';')
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for extension in [".COM", ".EXE", ".BAT", ".CMD"] {
        if !extensions
            .iter()
            .any(|value| value.eq_ignore_ascii_case(extension))
        {
            extensions.push(extension.into());
        }
    }
    env.insert(
        "PATHEXT".into(),
        ("PATHEXT".into(), extensions.join(";").into()),
    );
    // libuv supplies these additional OS basics even with an explicit env.
    // Preserve that observable legacy behavior without inheriting arbitrary
    // Agent credentials, proxies, NODE_OPTIONS or desktop session variables.
    for name in ["HOMEDRIVE", "HOMEPATH", "LOGONSERVER", "SYSTEMDRIVE"] {
        if let Some(value) = source.get(name) {
            env.entry(name.into())
                .or_insert_with(|| (name.into(), value.clone()));
        }
    }
    env
}

fn get_env<'a>(env: &'a Environment, name: &str) -> Option<&'a OsStr> {
    env.get(name).map(|(_, value)| value.as_os_str())
}
fn fallback(env: &mut Environment, name: &str, value: OsString) {
    let entry = env
        .entry(name.to_ascii_uppercase())
        .or_insert_with(|| (name.into(), OsString::new()));
    if entry.1.is_empty() {
        entry.1 = value;
    }
}
fn environment_block(env: &Environment) -> io::Result<Vec<u16>> {
    let mut block = Vec::new();
    for (name, value) in env.values() {
        let mut entry = OsString::from(name);
        entry.push("=");
        entry.push(value);
        block.extend(wide(&entry)?);
    }
    block.push(0);
    Ok(block)
}

/// Resolve the configured shell using the explicit child environment.
fn executable(shell: &str, cwd: &Path, env: &Environment) -> io::Result<PathBuf> {
    // libuv searches the child's cwd, then its explicit PATH. It tries the
    // supplied extension, .com and .exe, and does not use PATHEXT here.
    let candidate = |base: PathBuf| {
        let mut variants = Vec::with_capacity(3);
        if base.extension().is_some() {
            variants.push(base.clone());
        }
        for suffix in [".com", ".exe"] {
            let mut path = base.as_os_str().to_os_string();
            path.push(suffix);
            variants.push(PathBuf::from(path));
        }
        variants.into_iter().find(|path| path.is_file())
    };
    if shell.contains(['\\', '/', ':']) {
        return candidate(cwd.join(shell))
            .ok_or_else(|| io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32));
    }
    if let Some(path) = candidate(cwd.join(shell)) {
        return Ok(path);
    }
    if let Some(paths) = get_env(env, "PATH") {
        for path in std::env::split_paths(paths) {
            if let Some(path) = candidate(cwd.join(path).join(shell)) {
                return Ok(path);
            }
        }
    }
    Err(io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32))
}

struct Attributes(Vec<usize>);
impl Attributes {
    fn new() -> io::Result<Self> {
        let mut bytes = 0;
        // SAFETY: sizing query has no destination buffer.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes) };
        if io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || bytes == 0
            || bytes > 1024 * 1024
        {
            return Err(io::Error::other("Invalid process attribute storage size"));
        }
        let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        // SAFETY: aligned buffer has the size returned by the sizing query.
        if unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), 2, 0, &mut bytes)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }
    fn handles(&mut self, attribute: u32, handles: &[HANDLE]) -> io::Result<()> {
        // SAFETY: caller retains the slice and its owned handles until this
        // initialized attribute list has been deleted.
        if unsafe {
            UpdateProcThreadAttribute(
                self.0.as_mut_ptr().cast(),
                0,
                attribute as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: exactly one initialized attribute list lives in this buffer.
        unsafe { DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast()) };
    }
}

pub(super) fn spawn(mut plan: Plan, job: &Arc<Job>) -> io::Result<Child> {
    let executable = wide(executable(&plan.shell, &plan.cwd, &plan.environment)?.as_os_str())?;
    let cwd = wide(plan.cwd.as_os_str())?;
    let environment = environment_block(&plan.environment)?;
    // SAFETY: unnamed, non-inheritable job owned only by this supervisor.
    let raw_job = unsafe { CreateJobObjectW(null(), null()) };
    if raw_job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful creation transferred one real handle.
    let owned_job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the initialized limits and owned job remain live for the call.
    if unsafe {
        SetInformationJobObject(
            raw_job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let (stdin, child_stdin) = pipe::pair(true)?;
    let (stdout, child_stdout) = pipe::pair(false)?;
    let (stderr, child_stderr) = pipe::pair(false)?;
    let inherited = [
        child_stdin.as_raw_handle(),
        child_stdout.as_raw_handle(),
        child_stderr.as_raw_handle(),
    ];
    let jobs = [raw_job];
    // Drop order matters: attribute values and handles outlive the list.
    let mut attributes = Attributes::new()?;
    attributes.handles(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)?;
    attributes.handles(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[0];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[2];
    startup.lpAttributeList = attributes.0.as_mut_ptr().cast();
    let mut information = PROCESS_INFORMATION::default();
    if job.stop.load(Ordering::Acquire) || Instant::now() >= plan.deadline {
        return Err(cancelled_error());
    }
    // SAFETY: all pointers reference initialized, live storage. Only the three
    // pipe clients inherit. JOB_LIST attaches the child during creation, so
    // there is no unowned spawn/AssignProcessToJobObject crash interval.
    if unsafe {
        CreateProcessW(
            executable.as_ptr(),
            plan.command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW
                | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut information,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // No fallible operation follows creation. The already-allocated supervisor
    // must receive this owner even if cancellation or ResumeThread later fails.
    Ok(Child {
        job: owned_job,
        // SAFETY: CreateProcessW succeeded and transfers both handles.
        process: unsafe { OwnedHandle::from_raw_handle(information.hProcess) },
        thread: Some(unsafe { OwnedHandle::from_raw_handle(information.hThread) }),
        pid: information.dwProcessId,
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr: Some(stderr),
        directory: Some(plan.directory),
        deadline: plan.deadline,
    })
}

fn observe(process: &OwnedHandle) -> io::Result<Option<u32>> {
    // SAFETY: process identity is pinned by this owned handle, with no PID reuse.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
        WAIT_TIMEOUT => Ok(None),
        WAIT_OBJECT_0 => {
            let mut code = 0;
            // SAFETY: signaled process handle and writable exit-code result.
            if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                // 259 is a legitimate completed exit code. Liveness came from
                // WaitForSingleObject, never STILL_ACTIVE comparison.
                Ok(Some(code))
            }
        }
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ => Err(io::Error::other("Unexpected process wait state")),
    }
}
fn active_processes(job: &OwnedHandle) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: output has the exact requested accounting structure size.
    if unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(info.ActiveProcesses)
    }
}
fn failed(job: &Job, error: io::Error, syscall: &str) {
    let mut state = lock(&job.state);
    state.status = "failed";
    state.error = Some(os_error(&error, syscall));
    job.stop.store(true, Ordering::Release);
    job.changed.notify_all();
}

pub(super) fn supervise(job: &Job, mut child: Child, timeout: Duration, hook: Option<&ExitHook>) {
    let resumed = {
        // terminate() takes this same lock. Cancellation either wins before
        // resume, or observes a child that has already been allowed to run.
        let mut state = lock(&job.state);
        state.pid = Some(child.pid);
        if job.stop.load(Ordering::Acquire) || Instant::now() >= child.deadline {
            Err(cancelled_error())
        } else {
            // SAFETY: the owned primary thread was created suspended exactly once.
            match unsafe { ResumeThread(child.thread.as_ref().unwrap().as_raw_handle()) } {
                1 => Ok(()),
                u32::MAX => Err(io::Error::last_os_error()),
                _ => Err(io::Error::from_raw_os_error(ERROR_INVALID_STATE as i32)),
            }
        }
    };
    child.thread.take();
    child.directory.take();
    if let Err(error) = resumed {
        failed(job, error, "ResumeThread");
    }
    if job.stop.load(Ordering::Acquire) {
        child.stdin.take();
    }
    *lock(&job.stdin) = child.stdin.take();
    lock(&job.state).ready = true;
    job.changed.notify_all();
    let expires = Instant::now() + timeout;
    let mut decoders = [Decoder::default(), Decoder::default()];
    let mut last_terminate = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut stopping = false;
    loop {
        let observed = match observe(&child.process) {
            Ok(value) => value,
            Err(error) => {
                failed(job, error, "WaitForSingleObject");
                None
            }
        };
        if let Some(code) = observed {
            lock(&job.state).exit_code = Some(i64::from(code));
        }
        for result in [
            drain(&mut child.stdout, &mut decoders[0], job),
            drain(&mut child.stderr, &mut decoders[1], job),
        ] {
            if let Err(error) = result {
                failed(job, error, "ReadFile");
                child.stdout.take();
                child.stderr.take();
            }
        }
        stopping |= job.stop.load(Ordering::Acquire);
        if !stopping && observed.is_some() && child.stdout.is_none() && child.stderr.is_none() {
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
            if last_terminate.elapsed() >= Duration::from_millis(20) {
                // SAFETY: targets only the owned job, never an unpinned PID.
                if unsafe { TerminateJobObject(child.job.as_raw_handle(), 1) } == 0 {
                    failed(job, io::Error::last_os_error(), "TerminateJobObject");
                }
                last_terminate = Instant::now();
            }
            if observed.is_some() && child.stdout.is_none() && child.stderr.is_none() {
                match active_processes(&child.job) {
                    Ok(0) => {
                        // Only now can the owner handles, completion hook and
                        // admission slot close, in that order. Failed proof
                        // retains ownership and keeps cleanup_pending true.
                        drop(child);
                        finish(job, hook);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => failed(job, error, "QueryInformationJobObject"),
                }
            }
        }
        thread::sleep(TICK);
    }
}

#[cfg(test)]
#[path = "processes_windows_tests.rs"]
mod tests;
