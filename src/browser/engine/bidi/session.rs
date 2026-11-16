use super::{
    startup::{self, Boot},
    wire::{Bidi, Error, Events, Fault, Mode, websocket_config},
};
use crate::browser::process::firefox::OwnedFirefox;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Completion {
    Running,
    Stopped,
    CleanupPending,
}

pub(super) struct Managed {
    pub channel: Bidi,
    pub events: Option<Events>,
    pub boot: Option<Boot>,
    completion: watch::Receiver<Completion>,
}
impl Managed {
    pub async fn open(helper: &Path, executable: &Path, root: &Path) -> Result<Self, Error> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut owner = OwnedFirefox::spawn(helper, executable, root, None)
            .map_err(|_| Error::Transport(Fault::Startup))?;
        let ready = match owner.ready().await {
            Ok(ready) => ready,
            Err(_) => {
                owner
                    .close(false)
                    .await
                    .map_err(|_| Error::CleanupPending)?;
                return Err(Error::Transport(Fault::Startup));
            }
        };
        let socket = tokio::time::timeout_at(
            deadline.into(),
            tokio_tungstenite::client_async_with_config(
                &ready.url,
                ready.stream,
                Some(websocket_config()),
            ),
        )
        .await;
        let (socket, _) = match socket {
            Ok(Ok(socket)) => socket,
            _ => {
                owner
                    .close(false)
                    .await
                    .map_err(|_| Error::CleanupPending)?;
                return Err(Error::Transport(Fault::Startup));
            }
        };
        let (channel, events) = Bidi::new(socket);
        let (finished, completion) = watch::channel(Completion::Running);
        let native_channel = channel.clone();
        let mut lifecycle = channel.lifecycle();
        // This task owns the native process independently of every caller
        // future. Its lifetime ends only after native close/recovery status is
        // known. A consumer that never reads an event cannot prevent cleanup.
        tokio::spawn(async move {
            let graceful = loop {
                let mode = *lifecycle.borrow_and_update();
                match mode {
                    Mode::Running => {
                        if lifecycle.changed().await.is_err() {
                            break false;
                        }
                    }
                    Mode::Closing => break true,
                    Mode::Stopped(_) => break false,
                }
            };
            let result = owner.close(graceful).await;
            native_channel.stop();
            finished.send_replace(if result.is_ok() {
                Completion::Stopped
            } else {
                Completion::CleanupPending
            });
        });
        let mut this = Self {
            channel,
            events: Some(events),
            boot: None,
            completion,
        };
        match startup::initialize(&this.channel, this.events.as_mut().unwrap(), deadline).await {
            Ok(boot) => {
                this.boot = Some(boot);
                Ok(this)
            }
            Err(error) => {
                this.close(false).await?;
                Err(error)
            }
        }
    }

    pub async fn close(&mut self, graceful: bool) -> Result<(), Error> {
        if graceful {
            self.channel.graceful_stop();
        } else {
            self.channel.stop();
        }
        let deadline = Instant::now() + Duration::from_millis(3500);
        tokio::time::timeout_at(deadline.into(), async {
            while *self.completion.borrow_and_update() == Completion::Running {
                self.completion
                    .changed()
                    .await
                    .map_err(|_| Error::CleanupPending)?;
            }
            if *self.completion.borrow() == Completion::CleanupPending {
                return Err(Error::CleanupPending);
            }
            Ok(())
        })
        .await
        .map_err(|_| Error::CleanupPending)??;
        self.channel.stop();
        self.events.take();
        self.channel.wait_stopped(deadline).await
    }
}
impl Drop for Managed {
    fn drop(&mut self) {
        self.channel.stop();
    }
}

#[cfg(test)]
mod tests;
