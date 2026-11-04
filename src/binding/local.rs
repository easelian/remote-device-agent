use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    pid: u32,
    started: String,
    nonce: String,
    status: Value,
}

pub(super) fn instance_process(config: &Path) -> Result<Option<(u32, String)>> {
    read_optional(&sidecar(config, ".instance.json"))?
        .map(|raw| {
            let value: Instance = serde_json::from_slice(&raw)?;
            if value.nonce.len() != 32 {
                return Err(Error::message("Invalid Agent instance record"));
            }
            Ok((value.pid, value.started))
        })
        .transpose()
}

pub(super) fn report_existing(config: &Path) -> Result<bool> {
    let Some(raw) = read_optional(&sidecar(config, ".instance.json"))? else {
        return Ok(false);
    };
    let value: Instance = serde_json::from_slice(&raw)?;
    if !crate::process_identity::running(value.pid, &value.started)? {
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
    started: String,
    watcher: tokio::task::JoinHandle<()>,
}

impl Running {
    pub fn start(config: &Path, stop: watch::Sender<bool>) -> Result<Self> {
        let path = sidecar(config, ".instance.json");
        let nonce = crate::random::hex_token::<16>()?;
        let started = crate::process_identity::current()?;
        storage::write_private_replace(
            &path,
            &serde_json::to_vec(&Instance {
                pid: std::process::id(),
                started: started.clone(),
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
            started,
            watcher,
        })
    }

    pub fn status(&self, status: Value) -> Result<()> {
        storage::write_private_replace(
            &self.path,
            &serde_json::to_vec(&Instance {
                pid: std::process::id(),
                started: self.started.clone(),
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
