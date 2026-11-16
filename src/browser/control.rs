//! Session control epochs shared by browser backends and file transfers.
use super::{Failure, Result};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Whether an admitted command must settle before input changes hands.
/// Resource allocation and reads still retain their transport slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Page,
}

/// Authorization and operation progress shared by protocol adapters.
/// The adapter's pending table, not this value, owns submitted work.
#[derive(Clone, Default)]
pub struct Submission {
    authorization: Option<(Arc<Control>, u64, bool)>,
    effects: Vec<Arc<AtomicBool>>,
}

impl Submission {
    /// Delivery has no new effect, but must cross the same authorization fence
    /// as submission. The transport retains the original receipt on rejection.
    #[cfg(feature = "experimental-browser-bidi")]
    pub fn deliver<T>(&self, value: T) -> Result<T> {
        self.enqueue(Effect::None, || Ok(value))
    }
    pub fn authorized(&self, control: Arc<Control>, epoch: u64, paused: bool) -> Self {
        Self {
            authorization: Some((control, epoch, paused)),
            ..self.clone()
        }
    }

    pub fn track_effects(&self) -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let mut effects = self.effects.clone();
        effects.push(flag.clone());
        (
            Self {
                effects,
                ..self.clone()
            },
            flag,
        )
    }

    // D1a preserves the existing cleanup caller boundary. Resource-specific
    // cleanup permits belong to the later typed input/lease migration.
    pub fn cleanup(&self) -> Self {
        Self {
            authorization: None,
            ..self.clone()
        }
    }

    /// Check the epoch, enqueue and mark progress in the same fence interval.
    /// The closure must synchronously try to enqueue, roll back on failure and
    /// never wait for capacity, I/O or a response. An error means no enqueue.
    pub fn enqueue<T>(&self, effect: Effect, enqueue: impl FnOnce() -> Result<T>) -> Result<T> {
        let control = self
            .authorization
            .as_ref()
            .map(|(control, _, _)| control.state.lock().unwrap());
        if let Some((_, epoch, paused)) = &self.authorization
            && control
                .as_ref()
                .is_some_and(|state| state.0 != *epoch || state.1 != *paused)
        {
            return Err(Failure::new("browser_control_changed"));
        }
        let result = enqueue()?;
        if effect == Effect::Page {
            for effects in &self.effects {
                effects.store(true, Ordering::Release);
            }
        }
        Ok(result)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_input_tracking_preserves_the_operation_progress() {
        let (outer, operation) = Submission::default().track_effects();
        let (inner, input) = outer.track_effects();
        inner.enqueue(Effect::Page, || Ok(())).unwrap();
        assert!(operation.load(Ordering::Acquire));
        assert!(input.load(Ordering::Acquire));
    }

    #[test]
    fn changed_control_never_enters_the_enqueue_closure() {
        let control = Arc::new(Control::new());
        let (submission, effects) = Submission::default()
            .authorized(control.clone(), 1, false)
            .track_effects();
        control.change(true);
        let error = submission
            .enqueue::<()>(Effect::Page, || panic!("old epoch reached the queue"))
            .unwrap_err();
        assert_eq!(
            (error.code, error.state),
            ("browser_control_changed", "not_sent")
        );
        assert!(!effects.load(Ordering::Acquire));
    }

    #[test]
    fn queue_refusal_and_reads_do_not_create_effects_or_erase_prior_effects() {
        let (submission, effects) = Submission::default().track_effects();
        let refuse = || Err::<(), _>(Failure::new("browser_busy"));
        assert!(submission.enqueue(Effect::Page, refuse).is_err());
        assert!(!effects.load(Ordering::Acquire));
        assert_eq!(submission.enqueue(Effect::None, || Ok(7)).unwrap(), 7);
        assert!(!effects.load(Ordering::Acquire));
        submission.enqueue(Effect::Page, || Ok(())).unwrap();
        assert!(submission.enqueue(Effect::Page, refuse).is_err());
        submission.enqueue(Effect::None, || Ok(())).unwrap();
        assert!(effects.load(Ordering::Acquire));
    }

    #[test]
    fn fence_cannot_interleave_enqueue_and_progress() {
        let control = Arc::new(Control::new());
        let (submission, effects) = Submission::default()
            .authorized(control.clone(), 1, false)
            .track_effects();
        let (entered, at_queue) = std::sync::mpsc::channel();
        let (continue_enqueue, go) = std::sync::mpsc::channel();
        let submitter = std::thread::spawn(move || {
            submission.enqueue(Effect::Page, || {
                entered.send(()).unwrap();
                go.recv().unwrap();
                Ok(())
            })
        });
        at_queue.recv().unwrap();
        assert!(control.state.try_lock().is_err());
        assert!(!effects.load(Ordering::Acquire));
        let pauser = std::thread::spawn(move || {
            assert_eq!(control.change(true), 2);
            assert!(effects.load(Ordering::Acquire));
        });
        continue_enqueue.send(()).unwrap();
        submitter.join().unwrap().unwrap();
        pauser.join().unwrap();
    }

    #[test]
    fn human_authorization_requires_both_the_epoch_and_paused_state() {
        let control = Arc::new(Control::new());
        let wrong_state = Submission::default().authorized(control.clone(), 1, true);
        assert!(wrong_state.enqueue(Effect::Page, || Ok(())).is_err());
        let epoch = control.change(true);
        let (human, effects) = Submission::default()
            .authorized(control.clone(), epoch, true)
            .track_effects();
        human.enqueue(Effect::Page, || Ok(())).unwrap();
        control.resume(epoch).unwrap();
        assert!(human.enqueue(Effect::Page, || Ok(())).is_err());
        // The existing cleanup boundary must retain the operation's progress.
        human.cleanup().enqueue(Effect::None, || Ok(())).unwrap();
        assert!(effects.load(Ordering::Acquire));
    }
}
