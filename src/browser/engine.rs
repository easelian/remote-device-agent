//! Semantic ports used by page and session policy. Native encodings and handles
//! are private to the adapter; callers cannot construct or replay a wire command.
use super::{Result, control::Control};
use serde_json::Value;
use std::{
    future::Future,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

#[cfg(feature = "experimental-browser-bidi")]
pub(crate) mod bidi;
mod cdp;
pub use cdp::launch::detect;
#[cfg(feature = "experimental-browser-bidi")]
#[allow(unused_imports)] // Internal profile is selected only by D3 acceptance until D4.
pub(in crate::browser) use cdp::semantic as semantic_cdp;
#[cfg(test)]
pub mod testing;

#[derive(Clone)]
pub struct Engine {
    backend: cdp::Adapter,
}

// Values are owned by the handle, so replacing observations cannot grow a native
// ID registry. Scope distinguishes two connections even if a browser reuses IDs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Handle {
    scope: u64,
    key: Arc<str>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Context(Handle);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(Handle);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentId(Handle);
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TabId(Handle);

impl FrameId {
    /// Presentation at the frozen v1 boundary only; never resolves a handle.
    pub fn v1(&self) -> &str {
        &self.0.key
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    context: Context,
    key: i64,
}
#[derive(Clone)]
pub struct Frame {
    pub session: Context,
    pub id: FrameId,
    pub parent: Option<FrameId>,
    pub document: DocumentId,
    pub ancestors: Vec<(Context, FrameId)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
pub struct Hit {
    pub frame: FrameId,
    pub node: Node,
}

pub struct Launch {
    pub arguments: Vec<std::ffi::OsString>,
    #[cfg(unix)]
    pub child_descriptors: (i32, i32),
}
impl Launch {
    pub fn native(data: &Path) -> Self {
        cdp::launch::specification(data)
    }
    #[cfg(windows)]
    pub fn with_handles(&self, input: u32, output: u32) -> Vec<std::ffi::OsString> {
        cdp::launch::with_handles(&self.arguments, input, output)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NetworkId {
    context: Option<Context>,
    key: String,
}
#[derive(Clone)]
pub struct Request {
    _event: EventRetention,
    context: Option<Context>,
    key: String,
}
pub struct Stream {
    context: Option<Context>,
    key: String,
}
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub eof: bool,
}
#[derive(Clone)]
pub struct Header {
    pub name: String,
    pub value: String,
}
#[derive(Clone)]
pub struct DocumentRequest {
    pub handle: Request,
    pub context: Option<Context>,
    pub frame: FrameId,
    /// Names the connection whose address `Event::Connection` reports. Nothing
    /// compares the two yet: a policy that refuses private networks needs it,
    /// and sessions are not offered under such a policy.
    #[allow(dead_code)]
    pub network: NetworkId,
    pub url: String,
    pub responded: bool,
    pub redirected: bool,
    pub status: Option<u64>,
    pub headers: Option<Vec<Header>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    Page,
    Frame,
    Background,
}
pub struct TargetInfo {
    pub tab: TabId,
    pub root: FrameId,
    pub kind: ContextKind,
    pub url: String,
    pub title: String,
}
pub enum Event {
    Attached {
        context: Context,
        info: TargetInfo,
    },
    Detached(Context),
    TargetChanged(TargetInfo),
    DocumentPaused(DocumentRequest),
    #[allow(dead_code)] // See `DocumentRequest::network`.
    Connection {
        network: NetworkId,
        public: bool,
    },
    DialogOpened {
        context: Option<Context>,
        kind: Value,
        message: String,
    },
    DialogClosed {
        context: Option<Context>,
    },
}
pub enum RequestDecision {
    Continue,
    Deny,
    Abort,
}
/// The payload stays charged while an event handler or derived request owns it.
#[derive(Clone, Default)]
pub struct EventRetention {
    _lease: Option<cdp::EventLease>,
}
pub struct EventDelivery {
    pub event: Event,
    pub retention: EventRetention,
}
#[cfg(test)]
impl From<Event> for EventDelivery {
    fn from(event: Event) -> Self {
        Self {
            event,
            retention: EventRetention::default(),
        }
    }
}
pub struct Events {
    backend: cdp::Events,
}
impl Events {
    pub async fn recv(&mut self) -> Result<EventDelivery> {
        self.backend.recv().await
    }
}

/// Event handling can be tested with semantic events and a recording endpoint.
/// Maintenance is restricted to the resource carried by an event or lease.
pub trait SessionPort: Clone + Send + Sync + 'static {
    fn initialize_context(
        &self,
        context: &Context,
        downloads: bool,
        responses: bool,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
    fn resume_context(
        &self,
        context: &Context,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
    fn discard_tab(
        &self,
        tab: &TabId,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
    fn respond(
        &self,
        request: &Request,
        decision: RequestDecision,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
}
pub trait TransferPort: SessionPort {
    fn open_stream(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> impl Future<Output = Result<Stream>> + Send;
    fn read_stream(
        &self,
        stream: &Stream,
        limit: usize,
        deadline: Instant,
    ) -> impl Future<Output = Result<Chunk>> + Send;
    fn close_stream(
        &self,
        stream: Stream,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub trait PageReadPort: Sync {
    fn frames(
        &self,
        context: &Context,
        deadline: Instant,
    ) -> impl Future<Output = Result<Vec<Frame>>> + Send;
}

pub trait PageActionPort: PageReadPort {
    fn layout(
        &self,
        context: &Context,
        deadline: Instant,
    ) -> impl Future<Output = Result<Viewport>> + Send;
    fn frame_owner(
        &self,
        context: &Context,
        frame: &FrameId,
        deadline: Instant,
    ) -> impl Future<Output = Result<Node>> + Send;
    fn quad(&self, node: &Node, deadline: Instant)
    -> impl Future<Output = Result<[f64; 8]>> + Send;
    fn hit_at(
        &self,
        context: &Context,
        point: Point,
        deadline: Instant,
    ) -> impl Future<Output = Result<Hit>> + Send;
    fn scroll_into_view(
        &self,
        node: &Node,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
    fn click(
        &self,
        context: &Context,
        point: Point,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
    fn press(
        &self,
        context: &Context,
        key: &str,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
    fn attach_file(
        &self,
        node: &Node,
        path: &Path,
        deadline: Instant,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl Engine {
    #[cfg(test)]
    pub fn connected(&self) -> bool {
        self.backend.connected()
    }
    pub fn stop(&self) {
        self.backend.stop();
    }
    pub fn graceful_stop(&self) {
        self.backend.graceful_stop();
    }
}
