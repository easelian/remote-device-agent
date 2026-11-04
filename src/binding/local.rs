use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    pid: u32,
    nonce: String,
    status: Value,
}

pub(crate) fn alive(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Err(Error::message("Invalid Agent process identity"));
    }
    #[cfg(unix)]
    {
        if pid > i32::MAX as u32 {
            return Err(Error::message("Invalid Agent process identity"));
        }
        // Signal zero checks existence only; it never signals a reused PID.
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            #[cfg(target_os = "linux")]
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                && stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with('Z'))
            {
                return Ok(false);
            }
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(error.into()),
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
            System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
        };
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(false)
            } else {
                Err(error.into())
            };
        }
        let exited = unsafe { WaitForSingleObject(process, 0) } == WAIT_OBJECT_0;
        unsafe {
            CloseHandle(process);
        }
        Ok(!exited)
    }
    #[cfg(not(any(unix, windows)))]
    Err(Error::message(
        "Process inspection is unavailable on this platform",
    ))
}

pub(super) fn instance_pid(config: &Path) -> Result<Option<u32>> {
    read_optional(&sidecar(config, ".instance.json"))?
        .map(|raw| {
            let value: Instance = serde_json::from_slice(&raw)?;
            if value.nonce.len() != 32 {
                return Err(Error::message("Invalid Agent instance record"));
            }
            Ok(value.pid)
        })
        .transpose()
}

pub(super) fn report_existing(config: &Path) -> Result<bool> {
    let Some(raw) = read_optional(&sidecar(config, ".instance.json"))? else {
        return Ok(false);
    };
    let value: Instance = serde_json::from_slice(&raw)?;
    if !alive(value.pid)? {
        return Ok(false);
    }
    println!(
        "Agent is already running (PID {}).\n{}",
        value.pid,
        serde_json::to_string_pretty(&value.status)?
    );
    Ok(true)
}

pub(crate) struct Running {
    path: PathBuf,
    nonce: String,
    watcher: tokio::task::JoinHandle<()>,
}

impl Running {
    pub fn start(config: &Path, stop: watch::Sender<bool>) -> Result<Self> {
        let path = sidecar(config, ".instance.json");
        let nonce = crate::random::hex_token::<16>()?;
        storage::write_private_replace(
            &path,
            &serde_json::to_vec(&Instance {
                pid: std::process::id(),
                nonce: nonce.clone(),
                status: json!({"state":"starting"}),
            })?,
        )?;
        let config = config.to_path_buf();
        let watcher = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tick.tick().await;
                match pending_unbind(&config) {
                    Ok(false) => {}
                    Ok(true) => {
                        let _ = stop.send(true);
                        break;
                    }
                    Err(error) => {
                        eprintln!("Cannot inspect unbind state; stopping Agent: {error}");
                        let _ = stop.send(true);
                        break;
                    }
                }
            }
        });
        Ok(Self {
            path,
            nonce,
            watcher,
        })
    }

    pub fn status(&self, status: Value) -> Result<()> {
        storage::write_private_replace(
            &self.path,
            &serde_json::to_vec(&Instance {
                pid: std::process::id(),
                nonce: self.nonce.clone(),
                status,
            })?,
        )
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.watcher.abort();
        if let Ok(Some(raw)) = read_optional(&self.path)
            && serde_json::from_slice::<Instance>(&raw)
                .is_ok_and(|record| record.nonce == self.nonce)
        {
            let _ = remove_private(&self.path);
        }
    }
}
