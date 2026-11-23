//! Backend ownership and typed forwarding; the public runtime has no protocol API.
use crate::browser::{
    Result,
    control::Control,
    engine::{bidi::page as bidi, semantic_cdp as cdp},
    semantic::{
        self as sem,
        program::{Output, Program},
        surface::{Dialog, HumanInput, Metadata, Tab},
    },
};
use serde_json::Value;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

pub(super) enum Session {
    Cdp(cdp::Session),
    Bidi(bidi::Session),
}
#[derive(Clone)]
pub(super) enum Port {
    Cdp(cdp::Semantic),
    Bidi(bidi::Semantic),
}
impl Port {
    pub async fn visual_target_allowed(
        &self,
        frame: &sem::Frame,
        x: f64,
        y: f64,
        deadline: Instant,
    ) -> Result<bool> {
        match self {
            Self::Cdp(p) => p.visual_target_allowed(frame, x, y, deadline).await,
            Self::Bidi(p) => p.visual_target_allowed(frame, x, y, deadline).await,
        }
    }
}
impl Session {
    pub fn port(&self) -> Port {
        match self {
            Self::Cdp(s) => Port::Cdp(s.port.clone()),
            Self::Bidi(s) => Port::Bidi(s.port.clone()),
        }
    }
    pub async fn close(&mut self) -> Result<()> {
        match self {
            Self::Cdp(s) => s.close().await,
            Self::Bidi(s) => s.close().await,
        }
    }
}
macro_rules! forward {
    ($self:ident,$method:ident($($arg:expr),*)) => {match $self {Self::Cdp(p)=>p.$method($($arg),*),Self::Bidi(p)=>p.$method($($arg),*)}};
}
macro_rules! asynchronous {
    ($self:ident,$method:ident($($arg:expr),*)) => {match $self {Self::Cdp(p)=>p.$method($($arg),*).await,Self::Bidi(p)=>p.$method($($arg),*).await}};
}
impl sem::Port for Port {
    fn coordination(&self) -> Arc<sem::Coordination> {
        forward!(self, coordination())
    }
    fn authorize(&self, c: Arc<Control>, e: u64, h: bool) -> Self {
        match self {
            Self::Cdp(p) => Self::Cdp(p.authorize(c, e, h)),
            Self::Bidi(p) => Self::Bidi(p.authorize(c, e, h)),
        }
    }
    fn tracked(&self) -> (Self, Arc<AtomicBool>) {
        match self {
            Self::Cdp(p) => {
                let (p, f) = p.tracked();
                (Self::Cdp(p), f)
            }
            Self::Bidi(p) => {
                let (p, f) = p.tracked();
                (Self::Bidi(p), f)
            }
        }
    }
    fn connected(&self) -> bool {
        forward!(self, connected())
    }
    fn stop(&self) {
        forward!(self, stop())
    }
    fn pending(&self) -> usize {
        forward!(self, pending())
    }
    async fn frames(&self, d: Instant) -> Result<Vec<sem::Frame>> {
        asynchronous!(self, frames(d))
    }
    async fn refresh(&self, f: &sem::Frame, d: Instant) -> Result<sem::Frame> {
        asynchronous!(self, refresh(f, d))
    }
    async fn target(&self, f: &sem::Frame, n: u64, d: Instant) -> Result<sem::Geometry> {
        asynchronous!(self, target(f, n, d))
    }
    async fn program(&self, f: &sem::Frame, p: Program, d: Instant) -> Result<Output> {
        asynchronous!(self, program(f, p, d))
    }
    async fn native_accessibility(
        &self,
        f: &sem::Frame,
        n: u64,
        facts: &sem::evidence::Node,
        coverage: &mut sem::evidence::Coverage,
        d: Instant,
    ) -> Result<sem::accessibility::Snapshot> {
        asynchronous!(self, native_accessibility(f, n, facts, coverage, d))
    }
    async fn native_styles(
        &self,
        f: &sem::Frame,
        n: u64,
        property: sem::styles::StyleProperty,
        scope: sem::styles::StyleScope,
        d: Instant,
    ) -> Result<sem::styles::StyleSnapshot> {
        match self {
            Self::Cdp(p) => p.native_styles(f, n, property, scope, d).await,
            Self::Bidi(_) => Err(crate::browser::Failure::new("browser_feature_unsupported")),
        }
    }
    async fn input(&self, f: &sem::Frame, i: sem::NativeInput, d: Instant) -> Result<()> {
        asynchronous!(self, input(f, i, d))
    }
}
impl Port {
    pub async fn visual_hover(&self, x: f64, y: f64, deadline: Instant) -> Result<()> {
        asynchronous!(self, visual_hover(x, y, deadline))
    }
    pub async fn visual_drag(
        &self,
        x: f64,
        y: f64,
        to_x: f64,
        to_y: f64,
        deadline: Instant,
    ) -> Result<()> {
        asynchronous!(self, visual_drag(x, y, to_x, to_y, deadline))
    }
    pub async fn visual_click(&self, x: f64, y: f64, deadline: Instant) -> Result<()> {
        asynchronous!(self, visual_click(x, y, deadline))
    }
    pub async fn enable_task_events(
        &self,
        sink: crate::browser::semantic::task_events::Sink,
        d: Instant,
    ) -> Result<()> {
        match self {
            Self::Cdp(p) => p.enable_task_events(sink, d).await,
            Self::Bidi(p) => p.enable_task_events(sink, d).await,
        }
    }
    pub async fn response_body(&self, request: &str, d: Instant) -> Result<Value> {
        match self {
            Self::Cdp(p) => p.response_body(request, d).await,
            Self::Bidi(_) => Err(crate::browser::Failure::new("browser_feature_unsupported")),
        }
    }
    pub fn tab_id(&self) -> Result<String> {
        forward!(self, tab_id())
    }
    pub fn tabs(&self) -> Vec<Tab> {
        forward!(self, tabs())
    }
    pub fn for_tab(&self, id: &str) -> Result<Self> {
        match self {
            Self::Cdp(p) => Ok(Self::Cdp(p.for_tab(id)?)),
            Self::Bidi(p) => Ok(Self::Bidi(p.for_tab(id)?)),
        }
    }
    pub fn dialog(&self) -> Option<Dialog> {
        forward!(self, dialog())
    }
    pub async fn handle_dialog(&self, id: &str, accept: bool, d: Instant) -> Result<()> {
        asynchronous!(self, handle_dialog(id, accept, d))
    }
    pub async fn metadata_for(&self, frame: &sem::Frame, d: Instant) -> Result<Metadata> {
        asynchronous!(self, metadata_for(frame, d))
    }
    pub async fn metadata(&self, d: Instant) -> Result<(sem::Frame, Metadata)> {
        asynchronous!(self, metadata(d))
    }
    pub async fn focused_frame(&self, frame: &sem::Frame, d: Instant) -> Result<bool> {
        asynchronous!(self, focused_frame(frame, d))
    }
    pub async fn screenshot(&self, d: Instant) -> Result<Value> {
        asynchronous!(self, screenshot(d))
    }
    pub async fn navigate(&self, url: &str, d: Instant) -> Result<()> {
        asynchronous!(self, navigate(url, d))
    }
    pub async fn new_tab(&self, d: Instant) -> Result<String> {
        asynchronous!(self, new_tab(d))
    }
    pub async fn close_tab(&self, d: Instant) -> Result<()> {
        asynchronous!(self, close_tab(d))
    }
    pub async fn activate(&self, d: Instant) -> Result<()> {
        asynchronous!(self, activate(d))
    }
    pub async fn human_input(&self, i: HumanInput, d: Instant) -> Result<()> {
        asynchronous!(self, human_input(i, d))
    }
}
