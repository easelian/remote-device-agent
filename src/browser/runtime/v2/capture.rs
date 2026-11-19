//! Public diagnostic windows are bound to the authenticated session and the original document.
use super::*;
use sem::task_events::{MAX_EVENTS, Sink, WINDOW};

pub(super) struct Capture {
    pub(super) id: String,
    tab: String,
    epoch: u64,
    frame: sem::Frame,
    sink: Sink,
    action_owned: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.sink.invalidate();
    }
}

impl Capture {
    pub(super) fn stop_if_action_owned(&self) {
        if self.action_owned {
            self.sink.stop();
        }
    }

    pub(super) fn started(&self) -> Value {
        json!({"capture_id":self.id,"state":"active","start_sequence":1,
            "max_ms":WINDOW.as_millis(),"max_events":MAX_EVENTS})
    }

    pub(super) async fn start(
        session: &Session,
        slot: &Slot,
        port: &native::Port,
        tab: &str,
        epoch: u64,
        action: &Value,
        deadline: Instant,
        action_owned: bool,
    ) -> Result<Self> {
        let categories = action["categories"]
            .as_array()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let console = categories.iter().any(|v| v == "console");
        let network = categories.iter().any(|v| v == "network");
        let track_requests = action["track_requests"] == true;
        let track_exception_locations = action["track_exception_locations"] == true;
        if (track_requests && !network) || (track_exception_locations && !console) {
            return Err(Failure::new("browser_invalid_params"));
        }
        let (frame, _) = port.metadata(deadline).await?;
        if let Some(old) = &session.task_events
            && old.sink.active()
            && (old.tab != tab || old.frame == frame)
        {
            return Err(Failure::new("browser_capture_active"));
        }
        // This guard also invalidates a registered sink if the subsequent identity
        // check fails, the native call times out, or the caller drops this future.
        let capture = Capture {
            id: format!("cap_{}", token()?),
            tab: tab.to_owned(),
            epoch,
            frame,
            action_owned,
            sink: Sink::new_with_tracking(
                slot.control.clone(),
                epoch,
                console,
                network,
                track_requests,
                track_exception_locations,
            ),
        };
        port.enable_task_events(capture.sink.clone(), deadline)
            .await?;
        capture.check_document(port, deadline).await?;
        Ok(capture)
    }

    pub(super) async fn read_or_stop(
        session: &Session,
        port: &native::Port,
        tab: &str,
        epoch: u64,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        let capture = session
            .task_events
            .as_ref()
            .filter(|w| {
                w.tab == tab
                    && w.epoch == epoch
                    && params["capture_id"].as_str() == Some(w.id.as_str())
            })
            .ok_or_else(|| Failure::new("browser_capture_unavailable"))?;
        capture.check_document(port, deadline).await?;
        let kind = params["kind"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let mut value = match kind {
            "stop" => {
                // Confirm that the window has not been invalidated or retired.
                capture.sink.page(0, Some(0), 1)?;
                capture.sink.stop();
                json!({"state":"stopped"})
            }
            "read_events" => capture.sink.page(
                params["after_sequence"].as_u64().unwrap_or(0),
                params["through_sequence"].as_u64(),
                params["limit"].as_u64().unwrap_or(32) as usize,
            )?,
            "read_event" => json!({"event":capture.sink.event(params["sequence"].as_u64()
                .ok_or_else(|| Failure::new("browser_invalid_params"))?)?}),
            "read_exception_location" => capture.sink.exception_location(
                params["sequence"].as_u64().ok_or_else(|| Failure::new("browser_invalid_params"))?,
            )?,
            "read_requests" => {
                if !capture.sink.wants_progress() {
                    return Err(Failure::new("browser_capture_unavailable"));
                }
                capture.sink.requests()?
            }
            _ => return Err(Failure::new("browser_invalid_params")),
        };
        if kind != "stop" {
            value["kind"] = kind.into();
            value["content_trust"] = "untrusted_page".into();
        }
        capture.check_document(port, deadline).await?;
        value["capture_id"] = capture.id.clone().into();
        Ok(value)
    }

    async fn check_document(&self, port: &native::Port, deadline: Instant) -> Result<()> {
        if port.metadata(deadline).await?.0 != self.frame {
            self.sink.invalidate();
            return Err(Failure::new("browser_capture_unavailable"));
        }
        Ok(())
    }
}
