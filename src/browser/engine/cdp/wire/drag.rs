//! A Chromium pointer drag needs a second move at the same position before
//! its native drag target can be cancelled reliably. Reserve both commands
//! as one starting gesture. Release the immutable second frame to the pipe
//! after Chromium acknowledges the first, so it cannot coalesce the moves.
use super::*;

// The second command owns a slot from admission. If its sender is dropped
// before it reaches the pipe there can be no reply to release that slot.
struct Unsent<'a> {
    wire: &'a Cdp,
    id: Option<u64>,
}
impl Drop for Unsent<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.wire.inner.pending.lock().unwrap().commands.remove(&id);
        }
    }
}

impl Cdp {
    /// Reset a possibly held pointer only after Chromium cancels its drag.
    pub(in super::super) async fn cancel_drag(&self, session: &str) -> Result<()> {
        let cleanup = self.cleanup();
        let until = Instant::now() + Duration::from_secs(1);
        let result = async {
            cleanup.call(Some(session), Command::InputDragCancel, json!({}), until).await?;
            // Outside the document, including when revocation followed mouse-down
            // before Chromium had crossed its drag threshold.
            cleanup.call(Some(session), Command::InputDragReset,
                json!({"type":"mouseReleased","x":-1,"y":-1,"button":"left","buttons":0,"clickCount":0}),
                until).await
        }.await;
        if result.is_err() {
            self.stop();
            return Err(Failure::unknown("browser_cleanup_pending"));
        }
        Ok(())
    }

    pub(in super::super) async fn start_drag(
        &self,
        session: &str,
        x: f64,
        y: f64,
        deadline: Instant,
    ) -> Result<()> {
        let params = json!({"type":"mouseMoved","x":x,"y":y,"button":"left","buttons":1});
        let spec = Command::InputMouseMove.spec();
        let prepare = || {
            let permit = self
                .inner
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| Failure::new("browser_busy"))?;
            let writer = self
                .outgoing
                .clone()
                .try_reserve_owned()
                .map_err(|_| Failure::new("browser_busy"))?;
            let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
            let mut bytes = serde_json::to_vec(&json!({
                "sessionId":session,"id":id,"method":spec.method,"params":params
            }))
            .map_err(|_| Failure::new("browser_invalid_params"))?;
            bytes.push(0);
            let (reply, receive) = oneshot::channel();
            let pending = Pending {
                lease: leases::LeaseCommand::None,
                reply: Some(reply),
                session: Some(session.into()),
                input_barrier: true,
                dialog_blocks: true,
                cleanup_deadline: None,
                retired_deadline: None,
                _permit: permit,
                #[cfg(test)]
                metric: metrics::Command::new(Some(session), spec.method, &params),
            };
            Ok((id, pending, writer, bytes, receive))
        };
        let (first, first_command, first_writer, first_bytes, first_reply) = prepare()?;
        let (second, second_command, second_writer, second_bytes, second_reply) = prepare()?;
        self.submission.enqueue(spec.effect, || {
            let mut pending = self.inner.pending.lock().unwrap();
            if Instant::now() >= deadline {
                return Err(Failure::new("browser_deadline"));
            }
            if !self.connected() {
                return Err(Failure::unknown("browser_lost"));
            }
            if !pending.sessions.contains(session) {
                return Err(Failure::new("browser_stale_frame"));
            }
            if pending.dialogs.contains(session) {
                return Err(Failure::new("browser_dialog_open"));
            }
            pending.commands.insert(first, first_command);
            pending.commands.insert(second, second_command);
            first_writer.send(first_bytes);
            Ok(())
        })?;
        let mut unsent = Unsent {
            wire: self,
            id: Some(second),
        };
        let receive = |rx: oneshot::Receiver<Result<NativeReply>>| async move {
            tokio::time::timeout_at(deadline.into(), rx)
                .await
                .map_err(|_| Failure::unknown("browser_timeout_unknown"))?
                .map_err(|_| Failure::unknown("browser_lost"))?
                .map(drop)
        };
        let first_result = receive(first_reply).await;
        {
            let pending = self.inner.pending.lock().unwrap();
            let still_owned = self.connected()
                && pending.sessions.contains(session)
                && pending.commands.contains_key(&second)
                && !pending.dialogs.contains(session)
                && Instant::now() < deadline;
            if first_result.is_err() || !still_owned {
                return first_result.and(Err(Failure::unknown("browser_input_pending")));
            }
            second_writer.send(second_bytes);
            unsent.id = None;
        }
        receive(second_reply).await
    }
}
