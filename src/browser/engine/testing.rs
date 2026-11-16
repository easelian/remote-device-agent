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
    pub fn stable() -> Self {
        Self { scope: 0 }
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
    pub fn tab(&self, key: &str) -> TabId {
        TabId(self.handle(key))
    }
    pub fn frame_id(&self, key: &str) -> FrameId {
        FrameId(self.handle(key))
    }
    pub fn node(&self, context: &Context, key: i64) -> Node {
        Node {
            context: context.clone(),
            key,
        }
    }
    pub fn scan(&self, context: &Context, key: &str) -> Scan {
        Scan(Arc::new(ScanLease {
            context: context.clone(),
            object: key.into(),
            group: "fixture".into(),
            closed: AtomicBool::new(false),
        }))
    }
    pub fn frame(&self, context: &str, id: &str, document: &str, url: &str) -> Frame {
        Frame {
            session: self.context(context),
            id: self.frame_id(id),
            document: DocumentId(self.handle(document)),
            url: url.into(),
            fragment: String::new(),
            ancestors: Vec::new(),
        }
    }
    pub fn network(&self, context: &Context, key: &str) -> NetworkId {
        NetworkId {
            context: Some(context.clone()),
            key: key.into(),
        }
    }
    pub fn request(&self, context: &Context, key: &str) -> Request {
        Request {
            context: Some(context.clone()),
            key: key.into(),
        }
    }
}
