//! Linux-only process ownership experiment; no production Agent capability is enabled.
use super::{Result, cdp::Cdp};
use serde_json::{Value, json};
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt},
    process::CommandExt,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub struct Browser {
    pub child: Child,
    pub cdp: Cdp,
    pub session: String,
    pub startup_ms: f64,
    pub version: Value,
    profile: PathBuf,
    group: i32,
    stopped: bool,
}

fn group_members(group: i32) -> Vec<(i32, char)> {
    let mut members = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(") ") else {
                continue;
            };
            let fields: Vec<_> = fields.split_whitespace().collect();
            if fields.get(2).and_then(|v| v.parse::<i32>().ok()) == Some(group) {
                members.push((pid, fields[0].chars().next().unwrap_or('?')));
            }
        }
    }
    members
}

pub fn rss_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|s| s.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

impl Browser {
    pub async fn process_evidence(&self) -> Result<Value> {
        let process_info = self
            .cdp
            .call(None, "SystemInfo.getProcessInfo", json!({}))
            .await?;
        let processes = process_info["processInfo"]
            .as_array()
            .ok_or("missing_process_info")?;
        let members: Vec<_> = group_members(self.group).into_iter().map(|(pid, state)| {
            let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let renderer = processes.iter().any(|info| info["id"].as_f64() == Some(pid as f64) && info["type"] == "renderer");
            let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name)).unwrap_or("").trim().to_owned();
            json!({"pid":pid,"state":state.to_string(),"renderer":renderer,"seccomp":field("Seccomp:"),
                "cmdline_readable":!cmd.is_empty(),"no_new_privs":field("NoNewPrivs:"),"rss_kib":field("VmRSS:")})
        }).collect();
        Ok(json!({"group":self.group,"members":members,"no_sandbox_flag":false}))
    }

    pub async fn launch(executable: &Path, output: &Path, name: &str) -> Result<Self> {
        let started = Instant::now();
        let profile = output.join(format!("profile-{name}"));
        fs::DirBuilder::new().mode(0o700).create(&profile)?;
        // Adopt only descendants of this isolated harness, so killed browser children can be reaped.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stderr = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(output.join(format!("browser-{name}.stderr")))?;
        let mut child = Command::new(executable)
            .env_clear()
            .env("HOME", &profile)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .args([
                "--headless",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--disable-default-apps",
                "--disable-extensions",
                "--disable-breakpad",
                "--disable-crash-reporter",
                "--site-per-process",
                "--no-proxy-server",
                "--host-resolver-rules=MAP *.test 127.0.0.1",
                "--window-size=1280,720",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .process_group(0)
            .spawn()?;
        let group = child.id() as i32;
        let setup = async {
            let endpoint = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if child.try_wait()?.is_some() {
                        return Err("browser_start_failed_see_private_stderr".into());
                    }
                    if let Ok(text) = fs::read_to_string(profile.join("DevToolsActivePort")) {
                        let lines: Vec<_> = text.lines().collect();
                        if lines.len() >= 2 {
                            return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(format!(
                                "ws://127.0.0.1:{}{}",
                                lines[0], lines[1]
                            ));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await??;
            let cdp = Cdp::connect(&endpoint).await?;
            let version = cdp.call(None, "Browser.getVersion", json!({})).await?;
            let target = cdp
                .call(None, "Target.createTarget", json!({"url":"about:blank"}))
                .await?["targetId"]
                .as_str()
                .ok_or("missing_target")?
                .to_owned();
            let session = cdp
                .call(
                    None,
                    "Target.attachToTarget",
                    json!({"targetId":target,"flatten":true}),
                )
                .await?["sessionId"]
                .as_str()
                .ok_or("missing_session")?
                .to_owned();
            cdp.call(Some(&session), "Page.enable", json!({})).await?;
            cdp.call(Some(&session), "Runtime.enable", json!({}))
                .await?;
            cdp.call(
                Some(&session),
                "Target.setAutoAttach",
                json!({"autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true}),
            )
            .await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((cdp, version, session))
        }
        .await;
        match setup {
            Ok((cdp, version, session)) => Ok(Self {
                child,
                cdp,
                session,
                version,
                startup_ms: started.elapsed().as_secs_f64() * 1000.,
                profile,
                group,
                stopped: false,
            }),
            Err(error) => {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
                let _ = child.wait();
                reap(group);
                if group_members(group).is_empty() {
                    let _ = fs::remove_dir_all(profile);
                }
                Err(error)
            }
        }
    }

    pub async fn close(&mut self) -> Result<Value> {
        let started = Instant::now();
        let _ = self
            .cdp
            .call_for(None, "Browser.close", json!({}), Duration::from_secs(1))
            .await;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = self.child.try_wait()?;
            reap(self.group);
            if group_members(self.group).is_empty() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let forced = !group_members(self.group).is_empty();
        if forced {
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
        for _ in 0..100 {
            reap(self.group);
            if group_members(self.group).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !group_members(self.group).is_empty() {
            return Err("owned_processes_remain".into());
        }
        fs::remove_dir_all(&self.profile)?;
        self.stopped = true;
        Ok(
            json!({"milliseconds":started.elapsed().as_secs_f64()*1000.,"forced":forced,
            "remaining_group_members":0,"profile_removed":!self.profile.exists()}),
        )
    }
}

fn reap(group: i32) {
    // Child retains ownership of the group leader's wait status.
    for (pid, _) in group_members(group) {
        if pid != group {
            unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if !self.stopped {
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
            let _ = self.child.wait();
            for _ in 0..100 {
                reap(self.group);
                if group_members(self.group).is_empty() {
                    let _ = fs::remove_dir_all(&self.profile);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
