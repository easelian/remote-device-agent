//! Chromium adapter. Only this module knows protocol names, fields or identity
//! encodings. No raw-call API is exported to the semantic layer.
use super::*;
use crate::browser::{Failure, control::Effect};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(test)]
mod decode_tests;
pub(super) mod launch;
mod page;
#[cfg(feature = "experimental-browser-bidi")]
pub(in crate::browser) mod semantic;
mod session;
mod support;
mod wire;
pub(super) use session::Events;
use wire::{Cdp, Command, InputPair};

#[derive(Clone)]
struct Attachment {
    context: Context,
    parent: Option<Context>,
    root: FrameId,
}

static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);
#[derive(Clone)]
pub(super) struct Adapter {
    wire: Cdp,
    scope: u64,
    children: Arc<std::sync::Mutex<std::collections::HashMap<Context, Attachment>>>,
}
impl Adapter {
    #[cfg(test)]
    pub(super) fn scope(&self) -> u64 {
        self.scope
    }
    fn handle(&self, key: &str) -> Handle {
        Handle {
            scope: self.scope,
            key: key.into(),
        }
    }
    fn check(&self, handle: &Handle) -> Result<()> {
        if handle.scope != self.scope {
            return Err(Failure::new("browser_stale_reference"));
        }
        Ok(())
    }
    fn key<'a>(&self, handle: &'a Handle) -> Result<&'a str> {
        self.check(handle)?;
        Ok(&handle.key)
    }
    fn session<'a>(&self, context: &'a Context) -> Result<&'a str> {
        self.key(&context.0)
    }
    fn node(&self, node: &Node) -> Result<i64> {
        self.check(&node.context.0)?;
        Ok(node.key)
    }
    fn scan<'a>(&self, scan: &'a Scan) -> Result<&'a ScanLease> {
        self.check(&scan.0.context.0)?;
        if scan.0.closed.load(Ordering::Acquire) {
            return Err(Failure::new("browser_stale_reference"));
        }
        Ok(&scan.0)
    }
    pub(super) fn authorized(&self, control: Arc<Control>, epoch: u64, human: bool) -> Self {
        Self {
            scope: self.scope,
            children: self.children.clone(),
            wire: if human {
                self.wire.human(control, epoch)
            } else {
                self.wire.authorized(control, epoch)
            },
        }
    }
    pub(super) fn track_effects(&self) -> (Self, Arc<AtomicBool>) {
        let (wire, effect) = self.wire.track_effects();
        (
            Self {
                scope: self.scope,
                children: self.children.clone(),
                wire,
            },
            effect,
        )
    }
    pub(super) fn connected(&self) -> bool {
        self.wire.connected()
    }
    pub(super) fn pending(&self) -> usize {
        self.wire.pending()
    }
    pub(super) fn stop(&self) {
        self.wire.stop()
    }
    pub(super) fn graceful_stop(&self) {
        self.wire.graceful_stop()
    }
}
impl Engine {
    #[cfg(test)]
    pub(in crate::browser) fn test_metrics(&self) -> Value {
        self.backend.wire.test_metrics()
    }
    #[cfg(test)]
    pub(in crate::browser) fn raw_test_events(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.backend.wire.events()
    }
    #[cfg(test)]
    pub(in crate::browser) fn focus_program() -> &'static str {
        programs::INSERT
    }
    pub fn from_private_channel<R, W>(read: R, write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            backend: Adapter {
                wire: Cdp::new(read, write),
                scope: NEXT_SCOPE.fetch_add(1, Ordering::Relaxed),
                children: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            },
        }
    }
}

fn stale() -> Failure {
    Failure::new("browser_stale_reference")
}
fn actionable() -> Failure {
    Failure::new("browser_not_actionable")
}
/// A successful protocol envelope is not proof that its script ran. Keep the
/// original content-free failure vocabulary while rejecting malformed results.
fn script_value(reply: Value) -> Result<Value> {
    if reply.get("exceptionDetails").is_some() {
        return Err(stale());
    }
    reply
        .get("result")
        .and_then(|v| v.get("value"))
        .cloned()
        .ok_or_else(stale)
}
fn status(reply: Value) -> Result<ReaderStatus> {
    let value = script_value(reply)?;
    Ok(ReaderStatus {
        closed: value["closed"].as_bool().ok_or_else(stale)?,
        changed: value["changed"] == true,
        more: value["more"] == true,
    })
}
