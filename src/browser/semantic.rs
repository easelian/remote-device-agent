//! Explicitly selected semantic_dom_v1 profile. The public v1 path
//! keeps its own compatibility algorithm until capability v2 is negotiated.
#![allow(dead_code)] // Native probes and unsupported platform builds retain the shared contract.
pub(crate) mod actionability;
pub(crate) mod evidence;
mod page;
pub(crate) mod policy;
pub(crate) mod program;
pub(crate) mod surface;
pub(crate) mod types;
pub(crate) use page::Coordination;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use page::HumanPermit;
#[allow(unused_imports)]
pub(crate) use page::{Acknowledgement, Action, Continuation, Observation, Page, Reference};
pub(crate) use types::*;

use super::{Result, control::Control};
use std::{future::Future, sync::Arc, time::Instant};

/// The common coordinator consumes typed page facts and input intentions only.
/// Each adapter owns native identities and the shared transport submission gate.
pub(crate) trait Port: Clone + Send + Sync + 'static {
    fn coordination(&self) -> Arc<Coordination>;
    fn authorize(&self, control: Arc<Control>, epoch: u64, human: bool) -> Self;
    fn tracked(&self) -> (Self, Arc<std::sync::atomic::AtomicBool>);
    fn connected(&self) -> bool;
    fn stop(&self);
    fn pending(&self) -> usize;
    fn frames(&self, deadline: Instant) -> impl Future<Output = Result<Vec<Frame>>> + Send;
    fn refresh(
        &self,
        frame: &Frame,
        deadline: Instant,
    ) -> impl Future<Output = Result<Frame>> + Send;
    fn target(
        &self,
        frame: &Frame,
        node: u64,
        deadline: Instant,
    ) -> impl Future<Output = Result<Geometry>> + Send;
    fn program(
        &self,
        frame: &Frame,
        program: program::Program,
        deadline: Instant,
    ) -> impl Future<Output = Result<program::Output>> + Send;
    /// Own the entire native input sequence and release its held sources before
    /// returning. Page runs this future in a session-owned task, not the caller.
    fn input(
        &self,
        frame: &Frame,
        input: NativeInput,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
}

#[cfg(test)]
mod tests;
