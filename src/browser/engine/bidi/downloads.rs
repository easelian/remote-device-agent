//! Native Firefox attachments; one writer per session, bounded before continuation.
use super::{Command, ContextId, session::Cleanup, wire::Bidi};
use crate::browser::{
    Failure, Result, engine::Header, files, process::firefox::downloads::Directory,
};
use std::{
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

pub(super) struct Response {
    pub context: ContextId,
    pub navigation: Option<String>,
    pub url: String,
    pub status: u64,
}
struct Pending {
    context: ContextId,
    navigation: String,
    url: String,
    id: Option<String>,
    finish: Option<oneshot::Sender<Result<File>>>,
    done: Arc<AtomicBool>,
}
pub(super) struct Transfers {
    config: files::Downloads,
    directory: Arc<Directory>,
    cleanup: Cleanup,
    pending: Option<Pending>,
}
impl Transfers {
    pub fn new(config: files::Downloads, directory: Arc<Directory>, cleanup: Cleanup) -> Self {
        Self {
            config,
            directory,
            cleanup,
            pending: None,
        }
    }
    pub async fn response(
        &mut self,
        channel: &Bidi,
        request: Response,
        tab: String,
        headers: &[Header],
    ) -> Result<()> {
        let Response {
            context,
            navigation,
            url,
            status,
        } = request;
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.done.load(Ordering::Acquire))
        {
            self.pending = None;
        }
        if self.pending.is_some() {
            return Err(Failure::new("browser_file_limit"));
        }
        let navigation = navigation.ok_or_else(|| Failure::new("browser_download_unsupported"))?;
        let mut download = self.config.files.reserve(
            Some(status),
            headers,
            self.config.owner(tab, &url)?,
            true,
        )?;
        let (send, receive) = oneshot::channel();
        let done = Arc::new(AtomicBool::new(false));
        self.pending = Some(Pending {
            context,
            navigation,
            url,
            id: None,
            finish: Some(send),
            done: done.clone(),
        });
        let directory = self.directory.clone();
        let cleanup = self.cleanup.clone();
        let channel = channel.clone();
        let job = tokio::spawn(async move {
            let result = tokio::select! {
                value=receive=>value.unwrap_or_else(|_|Err(Failure::new("browser_file_incomplete"))),
                _=tokio::time::sleep(Duration::from_secs(120))=>Err(Failure::new("browser_timeout")),
                _=async{while !download.cancelled(){tokio::time::sleep(Duration::from_millis(25)).await;}}=>Err(Failure::new("browser_file_cancelled")),
            };
            let result = result.and_then(|file| download.complete(file));
            if let Err(error) = result {
                channel.stop();
                let stopped = cleanup.wait().await.is_ok();
                if stopped {
                    let _ = directory.clear();
                }
                download.fail(error.code, stopped);
            } else if directory.clear().is_err() {
                channel.stop();
                let _ = cleanup.wait().await;
            }
            done.store(true, Ordering::Release);
        });
        self.config.files.track(job);
        Ok(())
    }
    pub fn begin(
        &mut self,
        context: &ContextId,
        navigation: Option<&str>,
        url: &str,
        id: &str,
    ) -> Result<()> {
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| Failure::new("browser_download_unsupported"))?;
        if &pending.context != context
            || Some(pending.navigation.as_str()) != navigation
            || pending.url != url
            || pending.id.is_some()
        {
            return Err(Failure::new("browser_download_unsupported"));
        }
        pending.id = Some(id.into());
        Ok(())
    }
    pub fn end(
        &mut self,
        context: &ContextId,
        navigation: Option<&str>,
        url: &str,
        id: &str,
        path: Option<&str>,
    ) -> Result<()> {
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| Failure::new("browser_download_unsupported"))?;
        if &pending.context != context
            || Some(pending.navigation.as_str()) != navigation
            || pending.url != url
            || pending.id.as_deref() != Some(id)
        {
            return Err(Failure::new("browser_download_unsupported"));
        }
        let outcome = path
            .ok_or_else(|| Failure::new("browser_file_cancelled"))
            .and_then(|path| {
                self.directory
                    .open(path)
                    .map_err(|_| Failure::new("browser_file_incomplete"))
            });
        let sender = pending
            .finish
            .take()
            .ok_or_else(|| Failure::new("browser_download_unsupported"))?;
        sender
            .send(outcome)
            .map_err(|_| Failure::new("browser_file_cancelled"))
    }
}

pub(super) async fn behavior(channel: &Bidi, directory: Option<&Directory>) -> Result<()> {
    channel
        .call(
            Command::DownloadBehavior(directory.map(|d| d.path.clone())),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .map(drop)
        .map_err(super::page::failure)
}
