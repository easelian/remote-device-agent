//! An independent browser-target session owns one Runtime subscription. The
//! page-control session never enables Runtime or takes ownership of its events.
use super::console_route::Route;
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

const RELEASE_QUEUE: usize = 16;
const JOURNAL_RELEASE_QUEUE: usize = 256;
const SETTLE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

impl Cdp {
    pub(in crate::browser::engine) async fn start_console(
        &self,
        target: &str,
        sink: Sink,
        deadline: Instant,
    ) -> Result<()> {
        if target.is_empty() || target.len() > sessions::MAX_SESSION_ID_BYTES {
            return Err(Failure::new("browser_tab_unavailable"));
        }
        let gate = tokio::time::timeout_at(
            deadline.into(),
            self.inner.console_gate.clone().lock_owned(),
        )
        .await
        .map_err(|_| Failure::new("browser_busy"))?;
        let (release, objects) = mpsc::channel(if sink.journaling() {
            JOURNAL_RELEASE_QUEUE
        } else {
            RELEASE_QUEUE
        });
        let route = Arc::new(Route::new(target.to_owned(), sink.clone(), release));
        *self.inner.console.lock().unwrap() = Some(route.clone());
        let (ready, started) = oneshot::channel();
        let owner = self.clone();
        let target = target.to_owned();

        // The worker, rather than the caller's future, owns attachment and
        // teardown. A cancelled start must account for an already sent attach.
        tokio::spawn(async move {
            let _gate = gate;
            let reaper = tokio::spawn(
                owner
                    .cleanup()
                    .release_console_objects(route.clone(), objects),
            );
            let opened = owner.open_console(&target, &route, &sink, deadline).await;
            let ok = opened.is_ok();
            let delivered = ready.send(opened).is_ok();
            let live = ok && delivered;
            if live {
                while owner.connected() && sink.active() && !route.failed() {
                    tokio::time::sleep(POLL).await;
                }
            }
            route.closing();
            owner.detach_console(&route).await;
            reaper.abort();
            let _ = reaper.await;
            owner.inner.console.lock().unwrap().take();
        });
        tokio::time::timeout_at(deadline.into(), started)
            .await
            .map_err(|_| Failure::unknown("browser_timeout_unknown"))?
            .map_err(|_| Failure::unknown("browser_capture_unavailable"))?
    }

    async fn open_console(
        &self,
        target: &str,
        route: &Route,
        sink: &Sink,
        deadline: Instant,
    ) -> Result<()> {
        if !sink.active() {
            return Err(Failure::new("browser_capture_unavailable"));
        }
        let parent = match self
            .call(
                None,
                Command::TargetAttachToBrowserTarget,
                json!({}),
                deadline,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                // Without its session ID, an uncertain attach cannot be retired
                // individually. An acknowledged rejection allocates nothing.
                if error.state != "not_sent"
                    && error.code != "browser_command_failed"
                    && route.parent().is_none()
                {
                    self.stop();
                }
                return Err(error);
            }
        };
        let parent_id = route.parent();
        if parent_id.is_none() || parent["sessionId"].as_str() != parent_id.as_deref() {
            self.stop();
            return Err(Failure::unknown("browser_lost"));
        }
        if !sink.active() {
            return Err(Failure::new("browser_capture_unavailable"));
        }
        let subscribe = async {
            let reply = self
                .call(
                    parent_id.as_deref(),
                    Command::TargetAttachToTarget,
                    json!({"targetId":target,"flatten":true}),
                    deadline,
                )
                .await?;
            let child = route
                .child()
                .ok_or_else(|| Failure::new("browser_capture_unavailable"))?;
            if reply["sessionId"] != child || route.failed() || !sink.active() {
                return Err(Failure::new("browser_capture_unavailable"));
            }
            self.call(Some(&child), Command::RuntimeEnable, json!({}), deadline)
                .await?;
            Ok(())
        };
        // Once the owner ID is known we can detach the subtree even if the
        // child's attach or Runtime.enable reply is still pending.
        tokio::select! {
            result = subscribe => result?,
            _ = async { while sink.active() && !route.failed() { tokio::time::sleep(POLL).await; } } =>
                return Err(Failure::new("browser_capture_unavailable")),
        }
        if route.failed() || !sink.active() {
            return Err(Failure::new("browser_capture_unavailable"));
        }
        // Runtime.enable can replay entries from earlier grants. Arm only after
        // the acknowledgment and reject older native timestamps on late delivery.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Failure::new("browser_capture_unavailable"))?;
        if !route.arm(now.as_secs_f64() * 1000.0) {
            return Err(Failure::new("browser_capture_unavailable"));
        }
        Ok(())
    }

    async fn release_console_objects(self, route: Arc<Route>, mut objects: mpsc::Receiver<String>) {
        while let Some(object) = objects.recv().await {
            let Some(session) = route.child() else {
                route.fail();
                return;
            };
            if self
                .call(
                    Some(&session),
                    Command::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    Instant::now() + SETTLE,
                )
                .await
                .is_err()
            {
                route.fail();
                return;
            }
        }
    }

    async fn detach_console(&self, route: &Route) {
        let Some(parent) = route.parent() else { return };
        let wire = self.cleanup();
        let deadline = Instant::now() + SETTLE;
        while wire.connected() && wire.session_active(&parent) {
            let result = wire
                .call(
                    None,
                    Command::TargetDetachFromTarget,
                    json!({"sessionId":parent}),
                    deadline,
                )
                .await;
            match result {
                Ok(_) => break,
                Err(error)
                    if error.code == "browser_busy"
                        && error.state == "not_sent"
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(POLL).await;
                }
                Err(_) => break,
            }
        }
        while wire.connected() && wire.session_active(&parent) && Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
        }
        if wire.session_active(&parent) && wire.connected() {
            wire.stop();
        }
    }
}
