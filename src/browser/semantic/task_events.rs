//! A capture retains only fixed, value-only facts produced by a native adapter.
use super::super::{Failure, Result, control::Control};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

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
}
struct Inner {
    control: Arc<Control>,
    epoch: u64,
    started: Instant,
    console: bool,
    network: bool,
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
    pub(crate) fn new(control: Arc<Control>, epoch: u64, console: bool, network: bool) -> Self {
        Self(Arc::new(Inner {
            control,
            epoch,
            started: Instant::now(),
            console,
            network,
            window: Mutex::new(Window {
                entries: VecDeque::with_capacity(MAX_EVENTS),
                last: 0,
                stopped: None,
                invalid: false,
            }),
        }))
    }

    pub(crate) fn active(&self) -> bool {
        let now = Instant::now();
        let authority = self.0.control.state.lock().unwrap();
        let w = self.0.window.lock().unwrap();
        *authority == (self.0.epoch, false)
            && !w.invalid
            && w.stopped.is_none()
            && now.duration_since(self.0.started) < WINDOW
    }

    pub(crate) fn invalidate(&self) {
        let mut w = self.0.window.lock().unwrap();
        w.invalid = true;
        w.entries.clear();
    }

    pub(crate) fn stop(&self) {
        let mut w = self.0.window.lock().unwrap();
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
        let authority = self.0.control.state.lock().unwrap();
        if *authority != (self.0.epoch, false) {
            return;
        }
        let now = Instant::now();
        let mut w = self.0.window.lock().unwrap();
        if w.invalid || w.stopped.is_some() || now.duration_since(self.0.started) >= WINDOW {
            return;
        }
        if w.entries.len() == MAX_EVENTS {
            w.entries.pop_front();
        }
        w.last += 1;
        let sequence = w.last;
        w.entries.push_back(Entry {
            sequence,
            elapsed_ms: now.duration_since(self.0.started).as_millis() as u32,
            fact,
        });
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
