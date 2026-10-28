//! A single connection writer with message-count and byte backpressure.
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, Result};

pub const MAX_FRAME: usize = 2 * 1024 * 1024;
pub const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_QUEUED_MESSAGES: usize = 16;

pub struct Frame {
    pub message: Message,
    // Held until the actual socket send finishes or the queued frame is dropped.
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct Sender {
    tx: mpsc::Sender<Frame>,
    bytes: Arc<Semaphore>,
    failure: watch::Sender<bool>,
}

pub fn channel() -> (Sender, mpsc::Receiver<Frame>, watch::Receiver<bool>) {
    let (tx, rx) = mpsc::channel(MAX_QUEUED_MESSAGES);
    let (failure, failure_rx) = watch::channel(false);
    (
        Sender {
            tx,
            bytes: Arc::new(Semaphore::new(MAX_QUEUED_BYTES)),
            failure,
        },
        rx,
        failure_rx,
    )
}

impl Sender {
    /// True only when no frame is queued or being sent by the writer.
    pub fn idle(&self) -> bool {
        !self.tx.is_closed()
            && !*self.failure.borrow()
            && self.tx.capacity() == MAX_QUEUED_MESSAGES
            && self.bytes.available_permits() == MAX_QUEUED_BYTES
    }

    pub fn send(&self, message: Message) -> Result<()> {
        let bytes = match &message {
            Message::Text(value) => value.len(),
            Message::Binary(value) | Message::Ping(value) | Message::Pong(value) => value.len(),
            _ => 0,
        };
        let result = (|| {
            if bytes > MAX_FRAME {
                return Err(Error::message("Result exceeds the transport limit"));
            }
            let permit = Arc::clone(&self.bytes)
                .try_acquire_many_owned(bytes as u32)
                .map_err(|_| Error::message("Connection send budget exceeded"))?;
            self.tx
                .try_send(Frame {
                    message,
                    _bytes: permit,
                })
                .map_err(|_| Error::message("Connection send budget exceeded"))
        })();
        if result.is_err() {
            let _ = self.failure.send(true);
        }
        result
    }

    pub fn json(&self, value: &Value) -> Result<()> {
        self.send(Message::Text(serde_json::to_string(value)?.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slow_peer_cannot_grow_the_message_or_byte_queue() {
        let (sender, mut rx, failure) = channel();
        for _ in 0..MAX_QUEUED_MESSAGES {
            sender.send(Message::Text("x".into())).unwrap();
        }
        assert!(sender.send(Message::Text("overflow".into())).is_err());
        assert!(*failure.borrow());
        while rx.try_recv().is_ok() {}
        let (sender, _rx, failure) = channel();
        sender
            .send(Message::Binary(vec![0; MAX_FRAME].into()))
            .unwrap();
        sender
            .send(Message::Binary(vec![0; MAX_FRAME].into()))
            .unwrap();
        assert!(sender.send(Message::Binary(vec![0; 1].into())).is_err());
        assert!(*failure.borrow());
    }
}
