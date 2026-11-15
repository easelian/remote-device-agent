//! Session control epochs shared by browser backends and file transfers.
use super::{Failure, Result};
use std::sync::Mutex;

pub struct Control {
    // Serializes pause/close against the last authorization check and enqueue.
    pub state: Mutex<(u64, bool)>,
}
impl Control {
    pub fn new() -> Self {
        Self {
            state: Mutex::new((1, false)),
        }
    }
    pub fn change(&self, paused: bool) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        state.1 = paused;
        state.0
    }
    pub fn snapshot(&self) -> (u64, bool) {
        *self.state.lock().unwrap()
    }
    pub fn resume(&self, expected: u64) -> Result<u64> {
        let mut state = self.state.lock().unwrap();
        if state.0 != expected {
            return Err(Failure::new("browser_control_changed"));
        }
        state.0 += 1;
        state.1 = false;
        Ok(state.0)
    }
}
