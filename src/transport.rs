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
    pub(crate) fn fail(&self) {
        let _ = self.failure.send(true);
    }

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
            self.fail();
        }
        result
    }

    pub fn json(&self, value: &Value) -> Result<()> {
        self.send(Message::Text(serde_json::to_string(value)?.into()))
    }

    /// Replay already completed work through the same bounded writer. Waiting
    /// for capacity is cancellation safe: no frame is accepted until send wins.
    pub(crate) async fn json_wait(&self, value: &Value) -> Result<()> {
        self.send_wait(Message::Text(serde_json::to_string(value)?.into()))
            .await
    }

    /// A canceled wait accepts no frame and releases any reserved bytes.
    /// Closing the receiver also wakes byte waiters, including when a frame
    /// outside the queue still holds the entire byte budget.
    pub(crate) async fn send_wait(&self, message: Message) -> Result<()> {
        let bytes = match &message {
            Message::Text(value) => value.len(),
            Message::Binary(value) | Message::Ping(value) | Message::Pong(value) => value.len(),
            _ => 0,
        };
        if bytes > MAX_FRAME {
            return Err(Error::message("Result exceeds the transport limit"));
        }
        tokio::select! {
            _ = self.tx.closed() => Err(Error::message("Connection writer closed")),
            result = async {
                let permit = self.bytes.clone()
                    .acquire_many_owned(bytes as u32)
                    .await
                    .map_err(|_| Error::message("Connection writer closed"))?;
                self.tx.send(Frame { message, _bytes: permit })
                    .await
                    .map_err(|_| Error::message("Connection writer closed"))
            } => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_writer_wakes_byte_waiters_with_frames_still_in_flight() {
        let (sender, mut rx, _) = channel();
        for _ in 0..2 {
            sender
                .send(Message::Binary(vec![0; MAX_FRAME].into()))
                .unwrap();
        }
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        let waiting = sender.send_wait(Message::Text("blocked".into()));
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        rx.close();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .is_err()
        );
        drop((first, second));
        assert_eq!(sender.bytes.available_permits(), MAX_QUEUED_BYTES);
    }

    #[tokio::test]
    async fn canceling_message_wait_releases_reserved_bytes() {
        let (sender, mut rx, failure) = channel();
        for _ in 0..MAX_QUEUED_MESSAGES {
            sender.send(Message::Text("x".into())).unwrap();
        }
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                sender.send_wait(Message::Text("canceled".into())),
            )
            .await
            .is_err()
        );
        assert_eq!(
            sender.bytes.available_permits(),
            MAX_QUEUED_BYTES - MAX_QUEUED_MESSAGES
        );
        assert!(!*failure.borrow());
        while rx.try_recv().is_ok() {}
        assert!(sender.idle());
    }

    #[tokio::test]
    async fn replay_waits_for_capacity_without_failing_the_connection() {
        let (sender, mut rx, failure) = channel();
        for _ in 0..MAX_QUEUED_MESSAGES {
            sender.json(&serde_json::json!({"queued":true})).unwrap();
        }
        let replay = serde_json::json!({"replay":true});
        let waiting = sender.json_wait(&replay);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        assert!(!*failure.borrow());
        drop(rx.recv().await.unwrap());
        waiting.await.unwrap();
        assert!(!*failure.borrow());
        for _ in 0..MAX_QUEUED_MESSAGES - 1 {
            drop(rx.recv().await.unwrap());
        }
        let frame = rx.recv().await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(frame.message.to_text().unwrap()).unwrap(),
            replay
        );
        drop(frame);
        assert!(sender.idle());
    }

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
