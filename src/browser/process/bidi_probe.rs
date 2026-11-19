//! D2a Linux feasibility fixture. This module is absent from production builds.
//! It reuses the native Child owner, retirement, recovery and namespace locks.
//! The serial client is deliberately not the D2b concurrent transport.
use super::{Launch, platform, retire};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

mod client;
mod tests;
use client::{Client, Command, Endpoint};

type Result<T> = std::result::Result<T, &'static str>;

const STARTUP_LOG_LIMIT: u64 = 64 * 1024;
const STARTUP: Duration = Duration::from_secs(10);

struct ManagedFirefox {
    child: platform::Child,
    directory: PathBuf,
    owner: Value,
    endpoint: Option<Endpoint>,
    client: Option<Client>,
    closed: bool,
}

impl ManagedFirefox {
    fn spawn(executable: &Path, root: &Path) -> Result<Self> {
        let id = crate::random::hex_token::<16>().map_err(|_| "random_failed")?;
        let directory = root.join(id);
        crate::storage::create_private_dir(&directory).map_err(|_| "private_directory_failed")?;
        let data = directory.join("firefox-v1");
        crate::storage::create_private_dir(&data).map_err(|_| "private_directory_failed")?;
        crate::storage::write_private_new(&data.join("user.js"), PREFERENCES.as_bytes())
            .map_err(|_| "preferences_failed")?;
        let launch = Launch {
            arguments: vec![
                "--headless".into(),
                "--no-remote".into(),
                "--new-instance".into(),
                "--remote-debugging-port=0".into(),
                "--profile".into(),
                data.into_os_string(),
                "about:blank".into(),
            ],
            // The existing POSIX owner also allocates CDP descriptors. Firefox
            // does not consume them; their parent ends are immediately closed.
            // D2b must add an explicit channel variant before product wiring.
            child_descriptors: (3, 4),
        };
        let (child, read, write) = match platform::Child::spawn(executable, &directory, &launch) {
            Ok(value) => value,
            Err(_) => {
                let _ = fs::remove_dir_all(&directory);
                return Err("browser_start_failed");
            }
        };
        drop((read, write));
        let owner = child.owner();
        let mut this = Self {
            child,
            directory,
            owner,
            endpoint: None,
            client: None,
            closed: false,
        };
        if this.owner["start"].as_str().is_none_or(str::is_empty)
            || crate::storage::write_private_new(
                &this.directory.join("owner.json"),
                &serde_json::to_vec(&this.owner).unwrap(),
            )
            .is_err()
            || this.child.start().is_err()
        {
            return Err("owner_publication_failed");
        }
        Ok(this)
    }

    fn pid(&self) -> u32 {
        self.owner["pid"].as_u64().unwrap() as u32
    }

    async fn discover(&mut self, deadline: Instant) -> Result<()> {
        loop {
            if self.child.stopped().map_err(|_| "process_state_failed")? {
                return Err("browser_exited_before_endpoint");
            }
            let mut bytes = Vec::new();
            fs::File::open(self.directory.join("browser.stderr"))
                .map_err(|_| "startup_log_failed")?
                .take(STARTUP_LOG_LIMIT + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "startup_log_failed")?;
            if bytes.len() > STARTUP_LOG_LIMIT as usize {
                return Err("startup_log_limit");
            }
            if let Some(endpoint) = Endpoint::announcement(&bytes)? {
                if endpoint.owned_by(self.pid())? {
                    self.endpoint = Some(endpoint);
                    return Ok(());
                }
                return Err("endpoint_not_owned");
            }
            if Instant::now() >= deadline {
                return Err("startup_timeout");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn connect(&mut self, deadline: Instant) -> Result<Value> {
        let endpoint = self.endpoint.as_ref().ok_or("endpoint_missing")?;
        if !endpoint.owned_by(self.pid())? {
            return Err("endpoint_not_owned");
        }
        let mut client = Client::connect(endpoint, deadline).await?;
        let ready = client.request(Command::Status, deadline).await?;
        if ready["ready"] != true {
            return Err("session_not_ready");
        }
        let session = client.request(Command::NewSession, deadline).await?;
        let capabilities = &session["capabilities"];
        if capabilities["browserName"] != "firefox"
            || capabilities["browserVersion"]
                .as_str()
                .is_none_or(str::is_empty)
            || session["sessionId"].as_str().is_none_or(str::is_empty)
        {
            return Err("invalid_session_capabilities");
        }
        let product =
            json!({"name":capabilities["browserName"],"version":capabilities["browserVersion"]});
        self.client = Some(client);
        Ok(product)
    }

    async fn open(executable: &Path, root: &Path) -> Result<(Self, Value)> {
        let mut this = Self::spawn(executable, root)?;
        let deadline = Instant::now() + STARTUP;
        let result = async {
            this.discover(deadline).await?;
            this.connect(deadline).await
        }
        .await;
        match result {
            Ok(product) => Ok((this, product)),
            Err(error) => {
                this.close(false).await?;
                Err(error)
            }
        }
    }

    /// The native owner proves the complete process group stopped before the
    /// existing retirement function removes its private run directory.
    async fn close(&mut self, graceful: bool) -> Result<Value> {
        if self.closed {
            return Ok(json!({"already_closed":true}));
        }
        let started = Instant::now();
        let mut requested = false;
        if graceful && let Some(client) = self.client.as_mut() {
            requested = client
                .request(Command::CloseBrowser, started + Duration::from_secs(2))
                .await
                .is_ok();
        }
        // Keep the upgraded connection alive until Firefox has actually
        // stopped. A successful browser.close reply is not process exit.
        let mut forced = !graceful;
        let mut before_force = Value::Null;
        if forced {
            before_force = process_summary(self.pid())?;
            self.child.force_stop();
        }
        loop {
            if self
                .child
                .stopped()
                .map_err(|_| "browser_cleanup_pending")?
            {
                break;
            }
            if started.elapsed() >= Duration::from_secs(2) && !forced {
                before_force = process_summary(self.pid())?;
                forced = true;
                self.child.force_stop();
            }
            if started.elapsed() >= Duration::from_secs(3) {
                return Err("browser_cleanup_pending");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.client = None;
        self.owner["retired"] = true.into();
        crate::storage::write_private_replace(
            &self.directory.join("owner.json"),
            &serde_json::to_vec(&self.owner).unwrap(),
        )
        .map_err(|_| "browser_cleanup_pending")?;
        retire(&self.directory).map_err(|_| "browser_cleanup_pending")?;
        self.closed = true;
        Ok(
            json!({"requested_graceful":requested,"forced":forced,"before_force":before_force,
            "close_ms":started.elapsed().as_secs_f64()*1000.,
            "processes_remaining":members(self.pid())?.len(),
            "directory_remaining":self.directory.exists()}),
        )
    }
}

fn process_summary(group: u32) -> Result<Value> {
    Ok(members(group)?
        .into_iter()
        .map(|pid| {
            let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            };
            json!({"pid":pid,"name":field("Name:"),"state":field("State:"),"parent":field("PPid:")})
        })
        .collect::<Vec<_>>()
        .into())
}

impl Drop for ManagedFirefox {
    fn drop(&mut self) {
        if !self.closed {
            self.client = None;
            self.child.force_stop();
            for _ in 0..100 {
                if matches!(self.child.stopped(), Ok(true)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // Retain the record for the production recovery path. Never erase
            // a private directory merely because the caller dropped a future.
        }
    }
}

fn members(group: u32) -> Result<Vec<u32>> {
    let mut result = Vec::new();
    for entry in fs::read_dir("/proc").map_err(|_| "proc_unavailable")? {
        let entry = entry.map_err(|_| "proc_unavailable")?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
                continue;
            }
            Err(_) => return Err("process_state_failed"),
        };
        let fields = stat.rsplit_once(") ").ok_or("process_state_failed")?.1;
        if fields
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.parse::<u32>().ok())
            == Some(group)
        {
            result.push(pid);
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
"#;
