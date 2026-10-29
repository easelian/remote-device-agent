//! Configuration operations run as the service user, never as root. The child
//! holds the existing profile lock until the installer closes its input pipe.
use crate::{
    error::{Error, Result},
    profile_lock::ProfileLocks,
    startup::{Mode, Startup},
    storage,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub(super) struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}
impl Account {
    pub fn lookup(name: &str) -> Result<Self> {
        if name.is_empty() || name.len() > 256 || name.chars().any(|c| c.is_control() || c == ':') {
            return Err(Error::message("Invalid service user"));
        }
        let name_c = CString::new(name).map_err(|_| Error::message("Invalid service user"))?;
        let mut buffer = vec![0; 65536];
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: all output storage remains valid until its strings are copied.
        let status = unsafe {
            libc::getpwnam_r(
                name_c.as_ptr(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if status != 0 || found.is_null() {
            return Err(Error::message(
                "Service user does not exist or its account cannot be read",
            ));
        }
        let record = unsafe { record.assume_init() };
        if record.pw_uid == 0 || record.pw_gid == 0 || record.pw_dir.is_null() {
            return Err(Error::message(
                "Choose an existing non-root service account with a non-root primary group",
            ));
        }
        let home = unsafe { CStr::from_ptr(record.pw_dir) }
            .to_str()
            .map_err(|_| Error::message("Service home is not Unicode"))?;
        super::path_text(Path::new(home))?;
        Ok(Self {
            name: name.into(),
            uid: record.pw_uid,
            gid: record.pw_gid,
            home: home.into(),
        })
    }
    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("USER", &self.name)
            .env("LOGNAME", &self.name)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .current_dir("/");
        command
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ready {
    pub schema: u32,
    pub version: String,
    pub config: PathBuf,
    pub state: PathBuf,
    pub server: String,
    pub bound: bool,
}

pub(super) struct ProfileLease {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    pub ready: Ready,
}
impl ProfileLease {
    pub fn start(
        binary: &Path,
        account: &Account,
        config: &Path,
        state: &Path,
        name: &str,
        version: &str,
    ) -> Result<Self> {
        // Re-execute the already trusted installer, never the new candidate,
        // before resolving supplementary groups. NSS / OpenDirectory calls
        // are not safe inside pre_exec after a multithreaded runtime forks.
        let mut child = account
            .command(&std::env::current_exe()?)
            .args([
                "install-service",
                "--profile-exec",
                &account.name,
                &account.uid.to_string(),
                &account.gid.to_string(),
                super::path_text(binary)?,
                super::path_text(config)?,
                super::path_text(state)?,
                name,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        let input = child.stdin.take();
        let output = child
            .stdout
            .take()
            .ok_or_else(|| Error::message("Service profile output is unavailable"))?;
        let mut lease = Self {
            child,
            input,
            output,
            ready: Ready {
                schema: 0,
                version: String::new(),
                config: PathBuf::new(),
                state: PathBuf::new(),
                server: String::new(),
                bound: false,
            },
        };
        let flags = unsafe { libc::fcntl(lease.output.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe {
                libc::fcntl(
                    lease.output.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            } < 0
        {
            return Err(Error::message("Cannot read service profile validation"));
        }
        lease.ready = serde_json::from_slice(&lease.line()?)?;
        if lease.ready.schema != 1 || lease.ready.version != version {
            return Err(Error::message(
                "The candidate does not support the required service profile protocol or version",
            ));
        }
        super::path_text(&lease.ready.config)?;
        super::path_text(&lease.ready.state)?;
        crate::config::validate_gateway(&lease.ready.server)?;
        Ok(lease)
    }
    fn line(&mut self) -> Result<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0];
            match self.output.read(&mut byte) {
                Ok(0) => {
                    return Err(Error::message(
                        "Service profile validation ended without confirmation",
                    ));
                }
                Ok(_) if byte[0] == b'\n' => return Ok(bytes),
                Ok(_) => bytes.push(byte[0]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
            if bytes.len() > 16384 || Instant::now() >= deadline {
                return Err(Error::message(
                    "Service profile validation exceeded its output or time limit",
                ));
            }
        }
    }
    pub fn bind(&mut self) -> Result<()> {
        self.input
            .as_mut()
            .ok_or_else(|| Error::message("Service profile validation is closed"))?
            .write_all(b"bind\n")?;
        if self.line()? != b"bound" {
            return Err(Error::message("Service profile binding was not confirmed"));
        }
        self.ready.bound = true;
        Ok(())
    }
    pub fn check(&mut self) -> Result<()> {
        self.input
            .as_mut()
            .ok_or_else(|| Error::message("Service profile validation is closed"))?
            .write_all(b"check\n")?;
        if self.line()? != b"locked" {
            return Err(Error::message(
                "Service profile validation lost its configuration lock",
            ));
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<()> {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(Error::message("Service profile lock release failed"))
                };
            }
            if Instant::now() >= deadline {
                return Err(Error::message("Service profile lock release timed out"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for ProfileLease {
    fn drop(&mut self) {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            if Instant::now() >= deadline {
                // try_wait has not reaped the child, so it still pins this
                // process-group number while a blocked probe is terminated.
                unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// This synchronous command runs immediately after exec, without a Tokio
/// runtime. Only the trusted installer reaches this stage with root privileges;
/// the candidate is executed after all process credentials have been dropped.
pub(super) fn exec_as_user(args: &[String]) -> Result<()> {
    super::store::root()?;
    let [user, uid, gid, binary, config, state, name] = args else {
        return Err(Error::message("Invalid service credential arguments"));
    };
    let account = Account::lookup(user)?;
    if uid.parse::<u32>().ok() != Some(account.uid)
        || gid.parse::<u32>().ok() != Some(account.gid)
        || !crate::startup::valid_name(name)
    {
        return Err(Error::message("Service account or registration changed"));
    }
    for path in [binary, config, state] {
        super::path_text(Path::new(path))?;
    }
    let name_c =
        CString::new(account.name.as_str()).map_err(|_| Error::message("Invalid service user"))?;
    // initgroups also establishes Darwin's supplementary membership identity;
    // a hand-copied setgroups list loses that behavior and has a 16-group cap.
    // SAFETY: this newly executed helper has no concurrent Rust workers. The
    // account string lives through the call and credentials change only here.
    if unsafe { libc::initgroups(name_c.as_ptr(), account.gid as _) } != 0
        || unsafe { libc::setgid(account.gid) } != 0
        || unsafe { libc::setuid(account.uid) } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::getuid() } != account.uid
        || unsafe { libc::geteuid() } != account.uid
        || unsafe { libc::getgid() } != account.gid
        || unsafe { libc::getegid() } != account.gid
    {
        return Err(Error::message("Service credentials were not dropped"));
    }
    Err(account
        .command(Path::new(binary))
        .args(["install-service", "--profile-lease", config, state, name])
        .exec()
        .into())
}

pub(super) fn helper(args: &[String]) -> Result<()> {
    let [config, state, name] = args else {
        return Err(Error::message(
            "Invalid service profile validation arguments",
        ));
    };
    if unsafe { libc::getuid() } == 0 || !crate::startup::valid_name(name) {
        return Err(Error::message(
            "Service profile validation must run as the service user",
        ));
    }
    for path in [config, state] {
        super::path_text(Path::new(path))?;
    }
    use std::os::unix::fs::MetadataExt;
    let named = storage::private_metadata(Path::new(config), false)?;
    let config = Path::new(config).canonicalize()?;
    let state = Path::new(state).canonicalize()?;
    let canonical = storage::private_metadata(&config, false)?;
    if canonical.nlink() != 1 || named.nlink() != 1 {
        return Err(Error::message(
            "Service configuration cannot have hard links",
        ));
    }
    if named.dev() != canonical.dev() || named.ino() != canonical.ino() {
        return Err(Error::message(
            "Service configuration changed while resolving its path",
        ));
    }
    storage::private_metadata(
        config
            .parent()
            .ok_or_else(|| Error::message("Missing config parent"))?,
        true,
    )?;
    storage::private_metadata(&state, true)?;
    let mut locks = ProfileLocks::acquire(std::slice::from_ref(&config))?;
    let config_bytes = storage::read_private(&config)?;
    crate::app::validate_config(&["--config".into(), super::path_text(&config)?.into()])?;
    let paired = crate::config::DeviceConfig::read(&config)?;
    let (binding, _) = crate::startup::inspect_sidecar(&config)?;
    if binding
        .as_ref()
        .is_some_and(|b| b.mode != Mode::Service || b.name != *name)
    {
        return Err(Error::message(
            "The service profile is bound to another registration; migrate it explicitly",
        ));
    }
    let ready = Ready {
        schema: 1,
        version: env!("CARGO_PKG_VERSION").into(),
        config: config.clone(),
        state,
        server: paired.gateway_url,
        bound: binding.is_some(),
    };
    // After announcing readiness, an unexpected helper exit must retain the
    // legacy-compatible lock. Only the normal parent pipe close releases it.
    locks.retain_on_drop();
    serde_json::to_writer(std::io::stdout().lock(), &ready)?;
    println!();
    std::io::stdout().flush()?;
    let mut input = std::io::stdin().lock();
    loop {
        let mut line = Vec::new();
        loop {
            let mut byte = [0];
            if input.read(&mut byte)? == 0 {
                locks.release()?;
                return Ok(());
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
            if line.len() > 16 {
                return Err(Error::message("Invalid service profile command"));
            }
        }
        if storage::read_private(&config)? != config_bytes {
            return Err(Error::message(
                "Service configuration changed during validation",
            ));
        }
        if line == b"check" {
            println!("locked");
            std::io::stdout().flush()?;
            continue;
        }
        if line != b"bind" {
            return Err(Error::message("Invalid service profile command"));
        }
        Startup::new(&config)?
            .bind(
                Mode::Service,
                name,
                Instant::now() + Duration::from_secs(30),
                &std::sync::atomic::AtomicBool::new(false),
            )
            .map_err(|error| {
                Error::message(format!("Service binding failed: {}", error.message))
            })?;
        println!("bound");
        std::io::stdout().flush()?;
    }
}
