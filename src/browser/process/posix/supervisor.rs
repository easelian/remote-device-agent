//! One Linux subreaper per browser, including descendants that call setsid.
//! Only a childless supervisor may publish the durable completion proof.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child as Process, Command, Stdio},
    time::Duration,
};

const FLAG: &str = "--internal-cdp-supervisor";
const KIND: &str = "linux-cdp-supervisor-v1";
const IPC_FD: i32 = 5;
const CONFIG_LIMIT: usize = 32 * 1024;
const COMPLETE: &str = "supervisor-complete.json";
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    // AtomicBool is lock-free on the supported Linux targets. The handler does
    // no allocation, logging or I/O; the owner loop performs all cleanup.
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn prepare_signals() -> io::Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    // Inherited SIG_IGN or SA_NOCLDWAIT would release a child's PID before the
    // waitid-to-kill interval ends. This supervisor owns its reaping policy.
    action.sa_sigaction = libc::SIG_DFL;
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    action.sa_sigaction = request_stop as *const () as usize;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    executable: PathBuf,
    directory: PathBuf,
    arguments: Vec<OsString>,
    descriptors: (i32, i32),
}

pub struct Child {
    process: Process,
    channel: UnixStream,
    state: Box<State>,
    reaped: bool,
}

struct State {
    config: Vec<u8>,
    owner: Value,
    directory: PathBuf,
    activated: bool,
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn duplicate(fd: i32) -> io::Result<OwnedFd> {
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}
fn exited(pid: u32) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

impl Child {
    pub fn spawn(
        executable: &Path,
        profile: &Path,
        launch: &super::super::Launch,
    ) -> io::Result<(Self, tokio::net::UnixStream, tokio::net::UnixStream)> {
        let config = serde_json::to_vec(&Config {
            executable: executable.to_owned(),
            directory: profile.to_owned(),
            arguments: launch.arguments.clone(),
            descriptors: launch.child_descriptors,
        })?;
        if config.len() > CONFIG_LIMIT {
            return Err(other("browser_supervisor_config_limit"));
        }
        let (input, browser_input) = UnixStream::pair()?;
        let (output, browser_output) = UnixStream::pair()?;
        input.set_nonblocking(true)?;
        output.set_nonblocking(true)?;
        let input = tokio::net::UnixStream::from_std(input)?;
        let output = tokio::net::UnixStream::from_std(output)?;
        let (channel, child_channel) = UnixStream::pair()?;
        channel.set_write_timeout(Some(Duration::from_secs(1)))?;
        let sources = [
            duplicate(browser_input.as_raw_fd())?,
            duplicate(browser_output.as_raw_fd())?,
            duplicate(child_channel.as_raw_fd())?,
        ];
        let descriptors = sources.each_ref().map(AsRawFd::as_raw_fd);
        let stderr =
            crate::storage::create_private_file(&profile.join("guardian.stderr")).map_err(other)?;
        let mut command = helper_command()?;
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                for (source, target) in descriptors.into_iter().zip([3, 4, IPC_FD]) {
                    if libc::dup2(source, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let nonce = crate::random::hex_token::<16>().map_err(other)?;
        let process = command.spawn()?;
        let identity = crate::process_identity::started(process.id())
            .ok()
            .flatten()
            .unwrap_or_default();
        let owner = json!({"version":2,"kind":KIND,"pid":process.id(),"start":identity,
            "nonce":nonce});
        Ok((
            Self {
                process,
                channel,
                state: Box::new(State {
                    config,
                    owner,
                    directory: profile.to_owned(),
                    activated: false,
                }),
                reaped: false,
            },
            output,
            input,
        ))
    }
    pub fn owner(&self) -> Value {
        self.state.owner.clone()
    }
    pub fn start(&mut self) -> io::Result<()> {
        // The caller publishes owner.json before releasing this start gate.
        self.state.activated = true;
        self.channel
            .write_all(&(self.state.config.len() as u32).to_be_bytes())?;
        self.channel.write_all(&self.state.config)?;
        self.state.config = Vec::new();
        Ok(())
    }
    pub fn force_stop(&mut self) {
        // Do not kill the supervisor: it must retain and reap its descendants.
        let _ = self.channel.shutdown(Shutdown::Both);
    }
    pub fn stopped(&mut self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        if !exited(self.process.id())? {
            return Ok(false);
        }
        if self.state.activated && !completed(&self.state.directory, &self.state.owner)? {
            return Err(other("browser_cleanup_pending"));
        }
        self.process.wait()?;
        self.reaped = true;
        Ok(true)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            self.force_stop();
            let _ = self.process.try_wait();
        }
    }
}

fn helper_command() -> io::Result<Command> {
    #[cfg(test)]
    {
        if let Some(path) =
            std::env::var_os("RDH_BROWSER_HELPER").or_else(|| std::env::var_os("RDH_BIDI_HELPER"))
        {
            let mut command = Command::new(path);
            command.arg(FLAG);
            return Ok(command);
        }
        let mut command = Command::new(std::env::current_exe()?);
        command.args([
            "--exact",
            "browser::process::posix::supervisor::tests::helper_entry",
            "--ignored",
            "--test-threads=1",
        ]);
        Ok(command)
    }
    #[cfg(not(test))]
    {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg(FLAG);
        Ok(command)
    }
}

fn completed(directory: &Path, owner: &Value) -> io::Result<bool> {
    let bytes = match crate::storage::read_private(&directory.join(COMPLETE)) {
        Ok(value) => value,
        Err(crate::error::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(other(error)),
    };
    let proof: Value = serde_json::from_slice(&bytes)?;
    Ok(["version", "kind", "pid", "start", "nonce"]
        .into_iter()
        .all(|key| proof.get(key) == owner.get(key))
        && proof["complete"] == true)
}

pub fn retired(directory: &Path, owner: Option<&Value>) -> io::Result<bool> {
    let owner = owner.ok_or_else(|| other("Missing browser supervisor owner"))?;
    if owner["version"] != 2 || owner["kind"] != KIND {
        // Group-only journals cannot prove that detached descendants exited.
        return Err(other(
            "Legacy browser owner lacks descendant completion proof",
        ));
    }
    let pid = owner["pid"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v > 1 && *v <= i32::MAX as u32)
        .ok_or_else(|| other("Invalid browser supervisor owner"))?;
    let start = owner["start"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| other("Invalid browser supervisor identity"))?;
    if owner["nonce"]
        .as_str()
        .is_none_or(|s| s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(other("Invalid browser supervisor nonce"));
    }
    Ok(completed(directory, owner)?
        && crate::process_identity::started(pid)
            .map_err(other)?
            .as_deref()
            != Some(start))
}

pub(crate) fn run_helper() -> Option<io::Result<()>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|arg| arg != FLAG) {
        return None;
    }
    Some(if args.len() != 1 {
        Err(other("Invalid browser supervisor arguments"))
    } else {
        bootstrap()
    })
}

fn bootstrap() -> io::Result<()> {
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            IPC_FD,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } < 0
        || kind != libc::SOCK_STREAM
    {
        return Err(other("Invalid browser supervisor channel"));
    }
    let mut channel = unsafe { UnixStream::from_raw_fd(IPC_FD) };
    for fd in [3, 4, IPC_FD] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    channel.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut length = [0; 4];
    channel.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > CONFIG_LIMIT {
        return Err(other("browser_supervisor_config_limit"));
    }
    let mut bytes = vec![0; length];
    channel.read_exact(&mut bytes)?;
    let config: Config = serde_json::from_slice(&bytes)?;
    if !config.executable.is_absolute()
        || !config.directory.is_absolute()
        || config.descriptors != (3, 4)
    {
        return Err(other("Invalid browser supervisor config"));
    }
    crate::storage::private_metadata(&config.directory, true).map_err(other)?;
    let mut owner: Value = serde_json::from_slice(
        &crate::storage::read_private(&config.directory.join("owner.json")).map_err(other)?,
    )?;
    if owner["kind"] != KIND
        || owner["pid"].as_u64() != Some(std::process::id() as u64)
        || crate::process_identity::started(std::process::id())
            .map_err(other)?
            .as_deref()
            != owner["start"].as_str()
    {
        return Err(other("Invalid browser supervisor owner"));
    }
    prepare_signals()?;
    super::linux::prepare()?;
    let launched = launch_browser(&config);
    // No inherited parent-side descriptor may keep the CDP connection alive.
    unsafe {
        libc::close(3);
        libc::close(4);
    }
    let browser = launched.as_ref().ok().map(Process::id);
    if let Some(pid) = browser {
        owner["browser_pid"] = pid.into();
        let identity = crate::process_identity::started(pid)
            .map_err(other)
            .and_then(|value| value.ok_or_else(|| other("Browser identity unavailable")));
        let identity = match identity {
            Ok(value) => value,
            Err(error) => {
                let _ = channel.shutdown(Shutdown::Both);
                supervise(&channel, browser)?;
                return Err(error);
            }
        };
        owner["browser_start"] = identity.into();
        // If publication fails, retain this owner and stop its tree before
        // returning the failure; a metadata error cannot orphan the browser.
        if let Err(error) = crate::storage::write_private_replace(
            &config.directory.join("owner.json"),
            &serde_json::to_vec(&owner)?,
        ) {
            let _ = channel.shutdown(Shutdown::Both);
            supervise(&channel, browser)?;
            return Err(other(error));
        }
    } else if let Err(error) = &launched {
        eprintln!("Browser launch failed: {error}");
    }
    drop(launched);
    supervise(&channel, browser)?;
    let mut proof = owner;
    proof["complete"] = true.into();
    crate::storage::write_private_new(
        &config.directory.join(COMPLETE),
        &serde_json::to_vec(&proof)?,
    )
    .map_err(other)
}

fn launch_browser(config: &Config) -> io::Result<Process> {
    let stderr = crate::storage::create_private_file(&config.directory.join("browser.stderr"))
        .map_err(other)?;
    let mut command = Command::new(&config.executable);
    command
        .args(&config.arguments)
        .env_clear()
        .env("HOME", &config.directory)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir(&config.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .process_group(0);
    unsafe {
        command.pre_exec(|| {
            for fd in [3, 4] {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command.spawn()
}

fn supervise(channel: &UnixStream, browser: Option<u32>) -> io::Result<()> {
    let mut stopping = browser.is_none();
    loop {
        // This process has only descendants of its one browser. ECHILD is the
        // kernel's proof that the whole adopted tree has been collected.
        loop {
            let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if pid > 0 {
                if Some(pid as u32) == browser {
                    stopping = true;
                }
                continue;
            }
            if pid == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        stopping |= STOP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed);
        let mut wait = libc::pollfd {
            fd: channel.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut wait, 1, 10) } < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                stopping = true;
            }
        } else if wait.revents != 0 {
            stopping = true;
        }
        if stopping {
            stop_children()?;
        }
    }
}

fn stop_children() -> io::Result<()> {
    // The production helper is single-threaded. Enumerate all task child lists
    // as well so the same owner can run under Rust's test harness. Read bounded
    // prefixes; subsequent passes drain any remaining children and descendants.
    for task in fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024).read_to_end(&mut bytes)?;
        let length = bytes
            .iter()
            .rposition(u8::is_ascii_whitespace)
            .map_or(0, |i| i + 1);
        for token in bytes[..length]
            .split(u8::is_ascii_whitespace)
            .filter(|s| !s.is_empty())
        {
            let pid = std::str::from_utf8(token)
                .map_err(other)?
                .parse::<u32>()
                .map_err(other)?;
            if pid <= 1 || pid > i32::MAX as u32 {
                return Err(other("Invalid browser child identity"));
            }
            // waitid succeeds only for our own unreaped child. No other thread
            // reaps here, so its PID remains pinned through the signal.
            match exited(pid) {
                Ok(false) => {
                    if unsafe { libc::kill(pid as i32, libc::SIGKILL) } < 0
                        && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(true) => {}
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
