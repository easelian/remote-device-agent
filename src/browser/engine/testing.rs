//! Protocol-free opaque identities for deterministic semantic fixtures.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1 << 32);
#[derive(Clone, Copy)]
pub struct Ids {
    scope: u64,
}
impl Ids {
    pub fn new() -> Self {
        Self {
            scope: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }
    pub fn for_engine(engine: &Engine) -> Self {
        Self {
            scope: engine.backend.scope(),
        }
    }
    fn handle(&self, key: &str) -> Handle {
        Handle {
            scope: self.scope,
            key: key.into(),
        }
    }
    pub fn context(&self, key: &str) -> Context {
        Context(self.handle(key))
    }
    pub fn node(&self, context: &Context, key: i64) -> Node {
        Node {
            context: context.clone(),
            key,
        }
    }
}
