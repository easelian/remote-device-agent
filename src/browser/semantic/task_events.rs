//! A capture retains only fixed, value-only facts produced by a native adapter.
use super::super::{Failure, Result, control::Control};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) mod journal;
mod locations;
mod requests;
use locations::Locations;
use requests::Ledger;
pub(crate) use requests::{Method, RequestEndpoint, RequestUpdate, SubmitResponse};

pub(crate) const MAX_EVENTS: usize = 128;
pub(crate) const WINDOW: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Level {
    Log,
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKind {
    Dns,
    Timeout,
    Connection,
    Canceled,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Fact {
    Console(Level),
    Exception,
    Response(u16),
    Failed(FailureKind),
}

#[derive(Clone, Copy)]
struct Entry {
    sequence: u64,
    elapsed_ms: u32,
    fact: Fact,
}

// No event contains heap-owned native data. Even a full ring fits in 64 KiB.
const _: () = assert!(MAX_EVENTS * size_of::<Entry>() <= 64 * 1024);

impl Entry {
    fn public(self) -> Value {
        let mut value = match self.fact {
            Fact::Console(level) => json!({"kind":"console","level":level}),
            Fact::Exception => json!({"kind":"exception"}),
            Fact::Response(status) => json!({"kind":"response","status":status}),
            Fact::Failed(reason) => json!({"kind":"failed","reason":reason}),
        };
        value["sequence"] = self.sequence.into();
        value["elapsed_ms"] = self.elapsed_ms.into();
        value
    }
}

struct Window {
    entries: VecDeque<Entry>,
    last: u64,
    stopped: Option<Instant>,
    invalid: bool,
    requests: Option<Ledger>,
    locations: Option<Locations>,
    request_context: Option<Box<str>>,
    journal: Option<journal::Log>,
}
struct Inner {
    control: Arc<Control>,
    epoch: u64,
    started: Instant,
    console: bool,
    network: bool,
    progress: bool,
    track_submit: bool,
    /// A journal has no time limit and outlives the document it began on.
    endless: bool,
    location_start_ms: Option<u64>,
    window: Mutex<Window>,
}

#[derive(Clone)]
pub(crate) struct Sink(Arc<Inner>);
impl std::fmt::Debug for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sink").finish_non_exhaustive()
    }
}

impl Sink {
    #[cfg(test)]
    pub(crate) fn new(control: Arc<Control>, epoch: u64, console: bool, network: bool) -> Self {
        Self::new_with_progress(control, epoch, console, network, false)
    }

    #[cfg(test)]
    pub(crate) fn new_with_progress(
        control: Arc<Control>,
        epoch: u64,
        console: bool,
        network: bool,
        track: bool,
    ) -> Self {
        Self::new_with_tracking(control, epoch, console, network, track, false)
    }

    pub(crate) fn new_with_tracking(
        control: Arc<Control>,
        epoch: u64,
        console: bool,
        network: bool,
        track_requests: bool,
        track_exception_locations: bool,
    ) -> Self {
        Self::new_capture(
            control,
            epoch,
            console,
            network,
            track_requests,
            track_exception_locations,
            false,
        )
    }

    pub(crate) fn new_capture(
        control: Arc<Control>,
        epoch: u64,
        console: bool,
        network: bool,
        track_requests: bool,
        track_exception_locations: bool,
        track_submit: bool,
    ) -> Self {
        let progress = network && track_requests;
        let exception_locations = console && track_exception_locations;
        Self(Arc::new(Inner {
            control,
            epoch,
            started: Instant::now(),
            console,
            network,
            progress,
            track_submit,
            endless: false,
            location_start_ms: exception_locations
                .then(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .and_then(|d| u64::try_from(d.as_millis()).ok())
                })
                .flatten(),
            window: Mutex::new(Window {
                entries: VecDeque::with_capacity(MAX_EVENTS),
                last: 0,
                stopped: None,
                invalid: false,
                requests: progress.then(|| {
                    if track_submit {
                        Ledger::for_action_epoch(epoch)
                    } else {
                        Ledger::for_epoch(epoch)
                    }
                }),
                locations: exception_locations.then(Locations::default),
                request_context: None,
                journal: None,
            }),
        }))
    }

    /// A sink that keeps what the page at `page` and its successors in the
    /// same tab report, as text, for as long as this grant of control lasts.
    pub(crate) fn journal(control: Arc<Control>, epoch: u64, page: &str) -> Self {
        Self(Arc::new(Inner {
            control,
            epoch,
            started: Instant::now(),
            console: true,
            network: true,
            progress: false,
            track_submit: false,
            endless: true,
            location_start_ms: None,
            window: Mutex::new(Window {
                entries: VecDeque::new(),
                last: 0,
                stopped: None,
                invalid: false,
                requests: None,
                locations: None,
                request_context: None,
                journal: Some(journal::Log::on(page)),
            }),
        }))
    }

    pub(crate) fn journaling(&self) -> bool {
        self.0.endless
    }

    pub(crate) fn said(&self, said: journal::Said) {
        self.record(|w, _| {
            if let Some(log) = &mut w.journal {
                log.said(said);
            }
        });
    }

    pub(crate) fn net(&self, event: journal::Net) {
        self.record(|w, _| {
            if let Some(log) = &mut w.journal {
                log.net(event);
            }
        });
    }

    /// Reads the journal, unless control has changed hands since it began.
    pub(crate) fn journal_read<T>(&self, read: impl FnOnce(&mut journal::Log) -> T) -> Option<T> {
        let authority = self.0.control.state.lock().unwrap();
        if *authority != (self.0.epoch, false) {
            return None;
        }
        let mut w = self.0.window.lock().unwrap();
        w.journal.as_mut().map(read)
    }

    pub(crate) fn tracks_submit(&self) -> bool {
        self.0.track_submit
    }

    /// Set the backend-owned root frame/context used for private request
    /// correlation. It is never serialized in public task-event output.
    pub(crate) fn set_request_context(&self, context: &str) {
        if let Ok(mut w) = self.0.window.lock() {
            w.request_context = Some(context.into());
        }
    }

    /// Establish the exact action-submit barrier. Requests started before
    /// this call cannot later be attributed to the action.
    pub(crate) fn submit_dispatch(&self) -> Result<()> {
        let authority = self.0.control.state.lock().unwrap();
        let mut w = self.0.window.lock().unwrap();
        if *authority != (self.0.epoch, false)
            || w.invalid
            || w.stopped.is_some()
            || self.0.started.elapsed() >= WINDOW
        {
            return Err(Failure::new("browser_capture_unavailable"));
        }
        let Some(ledger) = w.requests.as_mut() else {
            return Err(Failure::new("browser_capture_unavailable"));
        };
        ledger.submit_dispatch();
        Ok(())
    }

    pub(crate) fn wants_console(&self) -> bool {
        self.0.console
    }

    pub(crate) fn wants_network(&self) -> bool {
        self.0.network
    }

    pub(crate) fn active(&self) -> bool {
        let now = Instant::now();
        let authority = self.0.control.state.lock().unwrap();
        let w = self.0.window.lock().unwrap();
        *authority == (self.0.epoch, false)
            && !w.invalid
            && w.stopped.is_none()
            && (self.0.endless || now.duration_since(self.0.started) < WINDOW)
    }

    pub(crate) fn invalidate(&self) {
        let mut w = self.0.window.lock().unwrap();
        w.invalid = true;
        w.entries.clear();
        w.requests = None;
        w.locations = None;
        w.journal = None;
    }

    pub(crate) fn invalidate_active(&self) {
        let mut window = self.0.window.lock().unwrap();
        if window.stopped.is_none() && (self.0.endless || self.0.started.elapsed() < WINDOW) {
            window.invalid = true;
            window.journal = None;
            window.entries.clear();
            window.requests = None;
            window.locations = None;
        }
    }

    pub(crate) fn stop(&self) {
        let mut w = self.0.window.lock().unwrap();
        if let Some(ledger) = &mut w.requests {
            ledger.forget_bindings();
        }
        if let Some(locations) = &mut w.locations {
            locations.forget_sources();
        }
        w.stopped
            .get_or_insert_with(|| Instant::now().min(self.0.started + WINDOW));
    }

    pub(crate) fn push(&self, fact: Fact) {
        let selected = match fact {
            Fact::Console(_) | Fact::Exception => self.0.console,
            Fact::Response(status) => self.0.network && (100..=599).contains(&status),
            Fact::Failed(_) => self.0.network,
        };
        if !selected {
            return;
        }
        self.record_event(fact, None);
    }

    pub(crate) fn wants_exception_locations(&self) -> bool {
        self.0.location_start_ms.is_some()
    }

    /// BiDi log times are Unix milliseconds. Exclude the ambiguous millisecond
    /// of registration, including buffered exceptions from the previous owner.
    pub(crate) fn contains_log_timestamp(&self, timestamp: &Value) -> bool {
        self.0
            .location_start_ms
            .zip(timestamp.as_u64())
            .is_some_and(|(start, event)| event > start)
    }

    pub(crate) fn push_exception(&self, stack: &Value, source_field: &str) {
        self.record_event(Fact::Exception, Some((stack, source_field)));
    }

    fn record_event(&self, fact: Fact, stack: Option<(&Value, &str)>) {
        self.record(|w, now| {
            if w.entries.len() == MAX_EVENTS
                && let Some(old) = w.entries.pop_front()
                && let Some(locations) = &mut w.locations
            {
                locations.evict(old.sequence);
            }
            w.last += 1;
            w.entries.push_back(Entry {
                sequence: w.last,
                elapsed_ms: now.duration_since(self.0.started).as_millis() as u32,
                fact,
            });
            if let (Some(locations), Some((native, field))) = (&mut w.locations, stack) {
                locations.push(w.last, native, field);
            }
        });
    }

    pub(crate) fn exception_location(&self, sequence: u64) -> Result<Value> {
        self.read(|w, _| {
            w.locations
                .as_ref()
                .and_then(|locations| locations.get(sequence))
                .ok_or_else(|| Failure::new("browser_capture_unavailable"))
        })
    }

    pub(crate) fn wants_progress(&self) -> bool {
        self.0.progress
    }

    pub(crate) fn progress(&self, id: &str, update: RequestUpdate) {
        self.record(|w, _| {
            if let Some(ledger) = &mut w.requests {
                ledger.update(id, update);
            }
        });
    }

    pub(crate) fn untracked(&self) {
        self.record(|w, _| {
            if let Some(ledger) = &mut w.requests {
                ledger.untracked += 1;
            }
        });
    }

    pub(crate) fn requests(&self) -> Result<Value> {
        self.read(|w, state| {
            w.requests
                .as_ref()
                .map(|l| l.snapshot(state))
                .ok_or_else(|| Failure::new("browser_capture_unavailable"))
        })
    }

    pub(crate) fn submit_response(
        &self,
        endpoint: &requests::RequestEndpoint,
        method: requests::Method,
    ) -> Result<requests::SubmitResponse> {
        self.read(|w, state| {
            w.requests
                .as_ref()
                .ok_or_else(|| Failure::new("browser_capture_unavailable"))
                .map(|ledger| {
                    ledger.submit_response(
                        endpoint,
                        method,
                        w.request_context.as_deref(),
                        state == "active",
                    )
                })
        })
    }

    fn record(&self, update: impl FnOnce(&mut Window, Instant)) {
        let authority = self.0.control.state.lock().unwrap();
        if *authority != (self.0.epoch, false) {
            return;
        }
        let now = Instant::now();
        let mut w = self.0.window.lock().unwrap();
        if w.invalid
            || w.stopped.is_some()
            || (!self.0.endless && now.duration_since(self.0.started) >= WINDOW)
        {
            return;
        }
        update(&mut w, now);
    }

    fn read<T>(&self, read: impl FnOnce(&Window, &'static str) -> Result<T>) -> Result<T> {
        let authority = self.0.control.state.lock().unwrap();
        let w = self.0.window.lock().unwrap();
        let now = Instant::now();
        let expires = self.0.started + WINDOW;
        if *authority != (self.0.epoch, false)
            || w.invalid
            || now >= w.stopped.unwrap_or(expires) + WINDOW
        {
            return Err(Failure::new("browser_capture_unavailable"));
        }
        let state = if w.stopped.is_some() {
            "stopped"
        } else if now >= expires {
            "expired"
        } else {
            "active"
        };
        read(&w, state)
    }

    pub(crate) fn page(&self, after: u64, through: Option<u64>, limit: usize) -> Result<Value> {
        let (entries, end, dropped, state) = self.read(|w, state| {
            let end = through.unwrap_or(w.last);
            if !(1..=64).contains(&limit) || after > end || end > w.last {
                return Err(Failure::new("browser_invalid_params"));
            }
            let entries: Vec<_> = w
                .entries
                .iter()
                .filter(|e| e.sequence > after && e.sequence <= end)
                .take(limit)
                .copied()
                .collect();
            let dropped = w.last - w.entries.len() as u64;
            Ok((entries, end, dropped, state))
        })?;
        // Advance across evicted events even if the entire fixed page was lost.
        let next = entries
            .last()
            .map_or(dropped.min(end).max(after), |e| e.sequence);
        Ok(
            json!({"events":entries.into_iter().map(Entry::public).collect::<Vec<_>>(),
            "state":state,"next_sequence":next,"through_sequence":end,"dropped_count":dropped}),
        )
    }

    pub(crate) fn event(&self, sequence: u64) -> Result<Value> {
        self.read(|w, _| {
            w.entries
                .iter()
                .find(|e| e.sequence == sequence)
                .copied()
                .ok_or_else(|| Failure::new("browser_capture_unavailable"))
        })
        .map(Entry::public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_keep_the_watermark_and_report_eviction() {
        let sink = Sink::new(Arc::new(Control::new()), 1, false, true);
        sink.push(Fact::Console(Level::Error));
        sink.push(Fact::Response(700));
        sink.push(Fact::Response(200));
        sink.push(Fact::Response(503));
        let first = sink.page(0, None, 1).unwrap();
        assert_eq!(first["through_sequence"], 2);
        assert_eq!(first["next_sequence"], 1);
        sink.push(Fact::Response(404));
        assert_eq!(
            sink.page(1, Some(2), 64).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for _ in 0..MAX_EVENTS {
            sink.push(Fact::Response(200));
        }
        let lost = sink.page(0, Some(2), 1).unwrap();
        assert_eq!(lost["next_sequence"], 2);
        assert_eq!(lost["dropped_count"], 3);
        assert!(sink.event(1).is_err());
    }
    #[test]
    fn buffered_bidi_exceptions_preceding_this_window_are_excluded() {
        let sink = Sink::new_with_tracking(Arc::new(Control::new()), 1, true, false, false, true);
        let cutoff = sink.0.location_start_ms.unwrap();
        assert!(!sink.contains_log_timestamp(&Value::Null));
        assert!(!sink.contains_log_timestamp(&json!(cutoff)));
        assert!(!sink.contains_log_timestamp(&json!(cutoff - 1)));
        assert!(sink.contains_log_timestamp(&json!(cutoff + 1)));
    }

    #[test]
    fn exception_location_tracks_mixed_event_eviction() {
        let sink = Sink::new_with_tracking(Arc::new(Control::new()), 1, true, false, false, true);
        let stack = json!({"callFrames":[{"source":"a.js","lineNumber":4,"columnNumber":8}]});
        sink.push_exception(&stack, "source");
        for _ in 0..MAX_EVENTS {
            sink.push(Fact::Console(Level::Info));
        }
        assert!(sink.exception_location(1).is_err());
    }

    #[test]
    fn exception_locations_bound_sources_and_reset_on_new_window() {
        let control = Arc::new(Control::new());
        let sink = Sink::new_with_tracking(control.clone(), 1, true, false, false, true);
        for n in 0..33 {
            let stack = json!({"callFrames":[{"source":format!("file-{n}.js"),"lineNumber":1,"columnNumber":1}]});
            sink.push_exception(&stack, "source");
        }
        let last = sink.exception_location(33).unwrap();
        assert_eq!(last["frames"][0]["source_id"], Value::Null);
        let known = json!({"callFrames":[{"source":"file-31.js","lineNumber":1,"columnNumber":1}]});
        sink.push_exception(&known, "source");
        assert_eq!(
            sink.exception_location(34).unwrap()["frames"][0]["source_id"],
            32
        );
        let fresh = Sink::new_with_tracking(control.clone(), 1, true, false, false, true);
        fresh.push_exception(&known, "source");
        assert_eq!(
            fresh.exception_location(1).unwrap()["frames"][0]["source_id"],
            1
        );
        sink.stop();
        assert!(sink.exception_location(33).is_ok());
        let epoch = control.change(true);
        assert!(epoch > 1);
        assert!(sink.exception_location(33).is_err());
    }

    #[test]
    fn malformed_exception_frames_are_omitted_and_marked_truncated() {
        let sink = Sink::new_with_tracking(Arc::new(Control::new()), 1, true, false, false, true);
        let stack = json!({"callFrames":[
            {"source":"x","lineNumber":-1,"columnNumber":2},
            {"source":"x","lineNumber":2,"columnNumber":3}
        ]});
        sink.push_exception(&stack, "source");
        let value = sink.exception_location(1).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(
            value["frames"],
            json!([{"source_id":1,"line":3,"column":4}])
        );
        let wide = json!({"parentId":{"id":"private"},"callFrames":[
            {"source":"x","lineNumber":0,"columnNumber":0},
            {"source":"too long".repeat(300),"lineNumber":0,"columnNumber":0},
            {"source":"x","lineNumber":2147483647,"columnNumber":0},
            {"source":"skipped","lineNumber":23,"columnNumber":25}]});
        sink.push_exception(&wide, "source");
        let value = sink.exception_location(2).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(
            value["frames"],
            json!([{"source_id":1,"line":1,"column":1},{"source_id":null,"line":1,"column":1}])
        );
        let parent = json!({"parent":{"callFrames":[]},"callFrames":[]});
        sink.push_exception(&parent, "source");
        assert_eq!(sink.exception_location(3).unwrap()["truncated"], true);
    }

    #[test]
    fn stop_freezes_and_control_transfer_revokes_a_window() {
        let control = Arc::new(Control::new());
        let sink = Sink::new(control.clone(), 1, true, true);
        sink.push(Fact::Exception);
        sink.stop();
        sink.push(Fact::Console(Level::Log));
        assert_eq!(sink.page(0, None, 10).unwrap()["state"], "stopped");
        assert_eq!(sink.event(1).unwrap()["kind"], "exception");
        assert!(sink.event(2).is_err());
        control.change(true);
        assert!(sink.event(1).is_err());
        let epoch = control.change(false);
        assert!(epoch > 1);
        assert!(sink.page(0, None, 1).is_err());
    }
}
