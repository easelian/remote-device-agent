//! Linux-only, explicitly gated BiDi process owner. No public backend selection.
//! PID-namespace init exit kills even descendants that changed process groups.
#![cfg_attr(not(test), allow(dead_code))]
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

mod ipc;
mod socks;
#[cfg(test)]
mod tests;

const FLAG: &str = "--internal-bidi-namespace";
const IPC_FD: i32 = 3;
const STARTUP: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    executable: PathBuf,
    directory: PathBuf,
    // Test-only barriers use the same inherited channel; no polling for stages.
    gate: Option<String>,
}

pub(crate) struct OwnedFirefox {
    child: Child,
    channel: tokio::io::unix::AsyncFd<OwnedFd>,
    directory: PathBuf,
    reaped: bool,
    egress: Option<socks::Egress>,
}

pub(crate) struct Ready {
    pub stream: tokio::net::TcpStream,
    pub url: String,
    pub port: u16,
}

impl OwnedFirefox {
    pub fn spawn(
        helper: &Path,
        executable: &Path,
        root: &Path,
        gate: Option<&str>,
    ) -> io::Result<Self> {
        let directory = root.join(crate::random::hex_token::<16>().map_err(other)?);
        crate::storage::create_private_dir(&directory).map_err(other)?;
        let result = Self::spawn_in(helper, executable, &directory, gate);
        if result.is_err() {
            // An owner record means native work may still exist. Recovery must
            // prove that it stopped; a spawn error alone is not that proof.
            if !directory.join("owner.json").exists() {
                let _ = fs::remove_dir_all(&directory);
            }
        }
        result
    }

    fn spawn_in(
        helper: &Path,
        executable: &Path,
        directory: &Path,
        gate: Option<&str>,
    ) -> io::Result<Self> {
        super::posix::linux::prepare()?;
        let (parent, child_end) = ipc::pair()?;
        let source = unsafe { libc::fcntl(child_end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        if source < 0 {
            return Err(io::Error::last_os_error());
        }
        let source = unsafe { OwnedFd::from_raw_fd(source) };
        let inherited = source.as_raw_fd();
        let parent_pid = unsafe { libc::getpid() };
        let stderr = crate::storage::create_private_file(&directory.join("guardian.stderr"))
            .map_err(other)?;
        let mut command = Command::new(helper);
        command
            .arg(FLAG)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(inherited, IPC_FD) < 0
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(io::Error::other("bidi_owner_gone"));
                }
                Ok(())
            });
        }
        // OwnedFd keeps the registered descriptor valid and stable throughout
        // AsyncFd's lifetime; no raw close is performed on the parent's end.
        let channel = unsafe { tokio::io::unix::AsyncFd::register(parent) }.map_err(other)?;
        let child = command.spawn()?;
        let identity = crate::process_identity::started(child.id())
            .map_err(other)
            .and_then(|value| value.ok_or_else(|| other("bidi_owner_missing")));
        // Construct the RAII owner before any fallible publication or await.
        let mut this = Self {
            child,
            channel,
            directory: directory.to_owned(),
            reaped: false,
            egress: None,
        };
        let setup = (|| {
            let start = identity?;
            let owner = json!({"version":2,"kind":"linux-bidi-namespace-v1","pid":this.child.id(),"start":start});
            crate::storage::write_private_new(
                &directory.join("owner.json"),
                &serde_json::to_vec(&owner)?,
            )
            .map_err(other)?;
            ipc::send(
                this.channel.get_ref().as_raw_fd(),
                &serde_json::to_value(Config {
                    executable: executable.to_owned(),
                    directory: directory.to_owned(),
                    gate: gate.map(str::to_owned),
                })?,
                &[],
            )
        })();
        if let Err(error) = setup {
            this.force_stop();
            return Err(error);
        }
        Ok(this)
    }

    pub async fn ready(&mut self) -> io::Result<Ready> {
        let deadline = tokio::time::Instant::now() + STARTUP;
        loop {
            let (message, mut fds) = tokio::time::timeout_at(deadline, ipc::read(&self.channel))
                .await
                .map_err(|_| other("bidi_startup_timeout"))??;
            match message["kind"].as_str() {
                Some("egress") if fds.len() == 1 && self.egress.is_none() => {
                    let listener = TcpListener::from(fds.remove(0));
                    listener.set_nonblocking(true)?;
                    self.egress = Some(socks::serve(tokio::net::TcpListener::from_std(listener)?));
                }
                Some("endpoint") if fds.len() == 1 && self.egress.is_some() => {
                    let port = message["port"]
                        .as_u64()
                        .and_then(|n| u16::try_from(n).ok())
                        .filter(|p| *p > 0)
                        .ok_or_else(|| other("bidi_invalid_endpoint"))?;
                    let stream = TcpStream::from(fds.remove(0));
                    if stream.peer_addr()? != std::net::SocketAddr::from(([127, 0, 0, 1], port)) {
                        return Err(other("bidi_invalid_endpoint"));
                    }
                    stream.set_nonblocking(true)?;
                    return Ok(Ready {
                        stream: tokio::net::TcpStream::from_std(stream)?,
                        url: format!("ws://127.0.0.1:{port}/session"),
                        port,
                    });
                }
                _ => return Err(other("bidi_invalid_startup")),
            }
        }
    }

    pub fn force_stop(&mut self) {
        // Child::kill targets an unreaped direct child, whose PID stays pinned.
        if !self.reaped {
            let _ = self.child.kill();
        }
        if let Some(egress) = self.egress.as_ref() {
            egress.stop();
        }
    }

    pub fn stopped(&mut self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        let pid = self.child.id();
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
        if unsafe { info.si_pid() } == 0 {
            return Ok(false);
        }
        if self.directory.join("owner.json").exists() {
            let owner: Value = serde_json::from_slice(
                &crate::storage::read_private(&self.directory.join("owner.json")).map_err(other)?,
            )?;
            if !retired(&owner)? {
                return Ok(false);
            }
        }
        // Pin the outer leader until adopted namespace-init zombies are reaped.
        // Descendants that escaped this group are covered by PID-init teardown.
        if !super::posix::linux::stopped(pid)? {
            return Ok(false);
        }
        self.child.wait()?;
        self.reaped = true;
        Ok(true)
    }

    pub async fn close(&mut self, graceful: bool) -> io::Result<()> {
        let started = Instant::now();
        if !graceful {
            self.force_stop();
        }
        loop {
            if self.stopped()? {
                break;
            }
            if started.elapsed() >= Duration::from_secs(2) {
                self.force_stop();
            }
            if started.elapsed() >= Duration::from_secs(3) {
                return Err(other("browser_cleanup_pending"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if let Some(egress) = self.egress.take() {
            egress.finish().await;
        }
        super::retire(&self.directory).map_err(other)
    }
}

impl Drop for OwnedFirefox {
    fn drop(&mut self) {
        self.force_stop();
        for _ in 0..20 {
            if self.reaped || matches!(self.stopped(), Ok(true)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Keep the journal; cancellation cannot certify native-tree exit.
    }
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

pub(super) fn retired(owner: &Value) -> io::Result<bool> {
    if owner["version"] != 2 || owner["kind"] != "linux-bidi-namespace-v1" {
        return Err(other("bidi_invalid_owner"));
    }
    for prefix in ["", "init_"] {
        let pid_key = format!("{prefix}pid");
        let start_key = format!("{prefix}start");
        if prefix == "init_" && owner.get(&pid_key).is_none() {
            continue;
        }
        let pid = owner[&pid_key]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .filter(|p| *p > 1)
            .ok_or_else(|| other("bidi_invalid_owner"))?;
        let start = owner[&start_key]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| other("bidi_invalid_owner"))?;
        if crate::process_identity::started(pid)
            .map_err(other)?
            .as_deref()
            == Some(start)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn run_helper() -> Option<io::Result<()>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|arg| arg != FLAG) {
        return None;
    }
    Some(if args.len() != 1 {
        Err(other("bidi_invalid_helper"))
    } else {
        bootstrap()
    })
}

fn bootstrap() -> io::Result<()> {
    // This entry runs before Tokio or any worker threads. All forks are here.
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
        || kind != libc::SOCK_SEQPACKET
    {
        return Err(other("bidi_invalid_helper_channel"));
    }
    unsafe {
        libc::fcntl(IPC_FD, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    wait_readable(IPC_FD, 10_000)?;
    let (value, fds) = ipc::receive(IPC_FD)?;
    if !fds.is_empty() {
        return Err(other("bidi_invalid_config"));
    }
    let config: Config = serde_json::from_value(value)?;
    if !config.executable.is_absolute() || !config.directory.is_absolute() {
        return Err(other("bidi_invalid_config"));
    }
    crate::storage::private_metadata(&config.directory, true).map_err(other)?;
    stage(&config, "before_namespace")?;
    let parent = unsafe { libc::getppid() };
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    if unsafe {
        libc::unshare(
            libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWPID | libc::CLONE_NEWNS,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    fs::write("/proc/self/setgroups", "deny")?;
    fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))?;
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } < 0
        || unsafe { libc::getppid() } != parent
    {
        return Err(other("bidi_owner_gone"));
    }
    // pidfd avoids the parent-died-before-prctl window in namespace PID 1,
    // where getppid() deliberately returns zero for an outside parent.
    let anchor_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) as i32 };
    if anchor_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let anchor_fd = unsafe { OwnedFd::from_raw_fd(anchor_fd) };
    let (gate_read, gate_write) = std::os::unix::net::UnixStream::pair()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        drop(gate_write);
        let result = (|| {
            if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut check = libc::pollfd {
                fd: anchor_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut check, 1, 0) } != 0 {
                return Err(other("bidi_anchor_gone"));
            }
            drop(anchor_fd);
            let mut gate = gate_read;
            let mut byte = [0];
            gate.read_exact(&mut byte)?;
            if byte != [1] {
                return Err(other("bidi_invalid_gate"));
            }
            drop(gate);
            namespace_init(&config)
        })();
        if let Err(error) = &result {
            eprintln!("BiDi namespace: {error}");
        }
        // Exiting PID 1 synchronously kills every member of this namespace.
        unsafe {
            libc::_exit(if result.is_ok() { 0 } else { 1 });
        }
    }
    drop(gate_read);
    drop(anchor_fd);
    let publication = (|| {
        let path = config.directory.join("owner.json");
        let mut owner: Value =
            serde_json::from_slice(&crate::storage::read_private(&path).map_err(other)?)?;
        owner["init_pid"] = (pid as u32).into();
        owner["init_start"] = crate::process_identity::started(pid as u32)
            .map_err(other)?
            .ok_or_else(|| other("bidi_init_missing"))?
            .into();
        crate::storage::write_private_replace(&path, &serde_json::to_vec(&owner)?)
            .map_err(other)?;
        (&gate_write).write_all(&[1])
    })();
    drop(gate_write);
    unsafe {
        libc::close(IPC_FD);
    }
    if publication.is_err() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    loop {
        if unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) } == pid {
            break;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
    publication
}

fn wait_readable(fd: i32, millis: i32) -> io::Result<()> {
    let mut wait = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut wait, 1, millis) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Err(other("bidi_wait_timeout"));
    }
    Ok(())
}

fn stage(config: &Config, name: &str) -> io::Result<()> {
    if config.gate.as_deref() == Some(name) {
        ipc::send(IPC_FD, &json!({"kind":"stage","stage":name}), &[])?;
        wait_readable(IPC_FD, 10_000)?;
        let (value, fds) = ipc::receive(IPC_FD)?;
        if value != json!({"continue":true}) || !fds.is_empty() {
            return Err(other("bidi_invalid_gate"));
        }
    }
    Ok(())
}

fn namespace_init(config: &Config) -> io::Result<()> {
    if unsafe { libc::getpid() } != 1 {
        return Err(other("bidi_missing_pid_namespace"));
    }
    if unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } < 0
        || unsafe {
            libc::mount(
                c"proc".as_ptr(),
                c"/proc".as_ptr(),
                c"proc".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // Bring up only the new namespace's loopback device. It has no veth, route
    // or host listener; all website TCP connections go through the inherited FD.
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if socket < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(socket) };
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    request.ifr_name[0] = b'l' as _;
    request.ifr_name[1] = b'o' as _;
    request.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_LOOPBACK) as _;
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS as _, &request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(socket);
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let proxy_port = listener.local_addr()?.port();
    ipc::send(IPC_FD, &json!({"kind":"egress"}), &[listener.as_raw_fd()])?;
    drop(listener);
    let data = config.directory.join("firefox-v1");
    crate::storage::create_private_dir(&data).map_err(other)?;
    let preferences =
        format!("{PREFERENCES}\nuser_pref(\"network.proxy.socks_port\", {proxy_port});\n");
    crate::storage::write_private_new(&data.join("user.js"), preferences.as_bytes())
        .map_err(other)?;
    stage(config, "before_browser")?;
    let stderr_path = config.directory.join("browser.stderr");
    let stderr = crate::storage::create_private_file(&stderr_path).map_err(other)?;
    let browser = Command::new(&config.executable)
        .args([
            "--headless",
            "--no-remote",
            "--new-instance",
            "--remote-debugging-port=0",
            "--profile",
        ])
        .arg(&data)
        .arg("about:blank")
        .env_clear()
        .env("HOME", &config.directory)
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .current_dir(&config.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()?;
    let browser_pid = browser.id() as i32;
    drop(browser);
    stage(config, "before_endpoint")?;
    let started = Instant::now();
    let mut published = false;
    loop {
        loop {
            let reaped = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if reaped == browser_pid {
                return Ok(());
            }
            if reaped == 0 {
                break;
            }
            if reaped < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if !published {
            let mut bytes = Vec::new();
            fs::File::open(&stderr_path)?
                .take(65537)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 65536 {
                return Err(other("bidi_startup_log_limit"));
            }
            if let Some(port) = endpoint(&bytes)? {
                let stream = TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    Duration::from_secs(1),
                )?;
                stage(config, "before_handshake")?;
                ipc::send(
                    IPC_FD,
                    &json!({"kind":"endpoint","port":port}),
                    &[stream.as_raw_fd()],
                )?;
                published = true;
            } else if started.elapsed() >= STARTUP {
                return Err(other("bidi_startup_timeout"));
            }
        }
        let mut lifeline = libc::pollfd {
            fd: IPC_FD,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut lifeline, 1, 10) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if lifeline.revents != 0 {
            return Err(other("bidi_owner_gone"));
        }
    }
}

fn endpoint(bytes: &[u8]) -> io::Result<Option<u16>> {
    let mut result = None;
    for line in bytes
        .split_inclusive(|b| *b == b'\n')
        .filter(|line| line.last() == Some(&b'\n'))
    {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if let Some(value) = line.trim().strip_prefix("WebDriver BiDi listening on ") {
            let url = url::Url::parse(value).map_err(|_| other("bidi_invalid_endpoint"))?;
            if url.scheme() != "ws"
                || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/"
            {
                return Err(other("bidi_invalid_endpoint"));
            }
            let port = url
                .port()
                .filter(|p| *p > 0)
                .ok_or_else(|| other("bidi_invalid_endpoint"))?;
            if result.replace(port).is_some_and(|prior| prior != port) {
                return Err(other("bidi_duplicate_endpoint"));
            }
        }
    }
    Ok(result)
}

const PREFERENCES: &str = r#"
user_pref("app.update.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.page", 0);
user_pref("browser.startup.homepage", "about:blank");
user_pref("browser.newtabpage.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("datareporting.healthreport.uploadEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("toolkit.telemetry.unified", false);
user_pref("network.captive-portal-service.enabled", false);
user_pref("network.connectivity-service.enabled", false);
user_pref("extensions.update.enabled", false);
user_pref("services.settings.server", "http://127.0.0.1:9/");
user_pref("network.proxy.type", 1);
user_pref("network.proxy.socks", "127.0.0.1");
user_pref("network.proxy.socks_version", 5);
user_pref("network.proxy.socks_remote_dns", true);
user_pref("network.proxy.no_proxies_on", "");
user_pref("network.proxy.allow_hijacking_localhost", true);
user_pref("network.proxy.failover_direct", false);
user_pref("network.trr.mode", 5);
user_pref("network.http.http3.enable", false);
"#;
