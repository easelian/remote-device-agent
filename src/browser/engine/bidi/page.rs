//! Native BiDi encoding for the shared semantic_dom_v1 coordinator.
#![cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)] // Public native sessions are validated on Linux x86_64.
#[cfg(target_os = "linux")]
use super::Event;
use super::{
    Command, ContextId,
    command::Action,
    wire::{Bidi, Error, ProtocolError},
};
use crate::browser::semantic::policy::Policy;
#[cfg(target_os = "linux")]
use crate::browser::semantic::policy::{Capabilities, Decisions};
use crate::browser::{
    Failure, Result,
    control::Control,
    semantic::{
        self as sem, Port,
        program::{Output, Program},
        types::Identities,
    },
};
use sem::surface::{Dialog, HumanInput, Metadata, Tab};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Clone)]
struct NativeTab {
    info: Tab,
    context: ContextId,
}
use std::{
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(in crate::browser) struct Semantic {
    channel: Bidi,
    root: ContextId,
    identities: Arc<Mutex<Identities>>,
    coordination: Arc<sem::Coordination>,
    tabs: Arc<Mutex<HashMap<String, NativeTab>>>,
    policy: Policy,
}

#[cfg(target_os = "linux")]
pub(in crate::browser) struct Session {
    pub port: Semantic,
    owner: super::session::Managed,
    task: tokio::task::JoinHandle<()>,
    decisions: Decisions,
}
#[cfg(target_os = "linux")]
impl Drop for Session {
    fn drop(&mut self) {
        self.port.coordination.close();
        self.port.channel.stop();
        self.task.abort();
    }
}
#[cfg(target_os = "linux")]
impl Session {
    pub async fn open(
        helper: &std::path::Path,
        browser: &std::path::Path,
        root: &std::path::Path,
        policy: Policy,
    ) -> Result<Self> {
        Self::open_with(
            helper,
            browser,
            root,
            policy,
            Arc::new(sem::Coordination::new()),
        )
        .await
    }
    pub async fn open_with(
        helper: &std::path::Path,
        browser: &std::path::Path,
        root: &std::path::Path,
        policy: Policy,
        coordination: Arc<sem::Coordination>,
    ) -> Result<Self> {
        Self::open_named(
            helper,
            browser,
            root,
            policy,
            coordination,
            Default::default(),
        )
        .await
    }
    pub async fn open_named(
        helper: &std::path::Path,
        browser: &std::path::Path,
        root: &std::path::Path,
        policy: Policy,
        coordination: Arc<sem::Coordination>,
        data: crate::browser::process::SessionData,
    ) -> Result<Self> {
        // This rejection precedes even native process/profile allocation.
        policy.admit(Capabilities {
            navigation_intercept: true,
            resolved_destination_check: true,
            observed_connection_address: false,
        })?;
        let transfers = data.downloads.clone();
        let mut owner = super::session::Managed::open_named(helper, browser, root, data)
            .await
            .map_err(failure)?;
        let channel = owner.channel.clone();
        let mut events = owner.events.take().unwrap();
        let boot = owner.boot.take().unwrap();
        if !boot.prompts_ignored {
            owner.close(false).await.map_err(failure)?;
            return Err(Failure::new("browser_backend_unsupported"));
        }
        let mut contexts = boot.contexts;
        let Some(root) = contexts.roots().first().cloned() else {
            owner.close(false).await.map_err(failure)?;
            return Err(sem::types::decode());
        };
        let tabs = Arc::new(Mutex::new(HashMap::new()));
        for context in contexts.roots() {
            let id = crate::random::hex_token::<16>().map_err(crate::browser::io_error)?;
            tabs.lock().unwrap().insert(
                id.clone(),
                NativeTab {
                    context,
                    info: Tab {
                        tab_id: id,
                        url: "about:blank".into(),
                        title: String::new(),
                        ready: true,
                        dialog: None,
                    },
                },
            );
        }
        super::downloads::behavior(&channel, owner.downloads.as_deref()).await?;
        let mut transfers = transfers
            .zip(owner.downloads.clone())
            .map(|(config, directory)| {
                super::downloads::Transfers::new(config, directory, owner.cleanup())
            });
        let observed = tabs.clone();
        let monitor = channel.clone();
        let rules = policy.clone();
        let decisions = Decisions::default();
        let ledger = decisions.clone();
        let task = tokio::spawn(async move {
            let mut decisions: tokio::task::JoinSet<std::result::Result<(), Error>> =
                tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    completed=decisions.join_next(),if !decisions.is_empty()=>{
                        if !matches!(completed,Some(Ok(Ok(())))) {monitor.stop();break;}
                    },
                    event=events.recv()=>match event {
                        Ok(Event::Request{context,request,url,navigation,hop,blocked})=>{
                            if !blocked {continue;}
                            if decisions.len()>=32 {monitor.stop();break;}
                            let known=context.as_ref().is_some_and(|value|contexts.ids().contains(value));
                            let token=match ledger.claim(context.as_ref().map_or("",ContextId::as_str),&request,hop,&url,navigation.is_some(),known){Ok(token)=>token,Err(_)=>{monitor.stop();break;}};
                            let channel=monitor.clone();let rules=rules.clone();
                            decisions.spawn(async move {
                                let allow=token.allows(&rules).await;
                                channel.call(Command::Decide{request:token.request().into(),allow},token.deadline()).await?;token.settled(allow);Ok(())
                            });
                        },
                        Ok(Event::Response{context,request,url,navigation,status,headers,blocked})=>{
                            if !blocked {continue;}
                            let headers=headers.into_iter().map(|(name,value)|crate::browser::engine::Header{name,value}).collect::<Vec<_>>();
                            let allow=if crate::browser::files::attachment(&headers) {
                                let tab=observed.lock().unwrap().values().find(|t|Some(&t.context)==context.as_ref()).map(|t|t.info.tab_id.clone());
                                if let (Some(transfers),Some(context),Some(tab))=(transfers.as_mut(),context,tab) {
                                    transfers.response(&monitor,super::downloads::Response{context,navigation,url,status},tab,&headers).await.is_ok()
                                } else {false}
                            } else {true};
                            if monitor.call(Command::Respond{request,allow},Policy::deadline()).await.is_err(){monitor.stop();break;}
                        },
                        Ok(Event::Download{context,navigation,url,id,end,path})=>{
                            let result=if let Some(transfers)=transfers.as_mut(){
                                if end {transfers.end(&context,navigation.as_deref(),&url,&id,path.as_deref())}
                                else {transfers.begin(&context,navigation.as_deref(),&url,&id)}
                            }else{Err(Failure::new("browser_download_unsupported"))};
                            if result.is_err(){monitor.stop();break;}
                        },
                        Ok(event@Event::Created{..})=>{
                            ledger.context_created();if !contexts.apply(&event){monitor.stop();break;}
                            if let Event::Created{context,parent:None}=&event {
                                if observed.lock().unwrap().len()>=8 {
                                    if monitor.call(Command::CloseContext(context.clone()),Policy::deadline()).await.is_err(){monitor.stop();break;}
                                } else if !observed.lock().unwrap().values().any(|t|&t.context==context) {
                                    let id=match crate::random::hex_token::<16>(){Ok(id)=>id,Err(_)=>{monitor.stop();break;}};
                                    observed.lock().unwrap().insert(id.clone(),NativeTab{context:context.clone(),info:Tab{tab_id:id,url:"about:blank".into(),title:String::new(),ready:true,dialog:None}});
                                }
                            }
                        },
                        Ok(event@Event::Destroyed{..})=>{
                            if let Event::Destroyed{context}=&event {observed.lock().unwrap().retain(|_,t|&t.context!=context);}
                            if !contexts.apply(&event){monitor.stop();break;}
                        },
                        Ok(Event::Prompt{context,kind,message})=>{
                            let dialog=match Dialog::new(&kind,&message){Ok(d)=>d,Err(_)=>{monitor.stop();break;}};
                            let mut all=observed.lock().unwrap();
                            let Some(tab)=all.values_mut().find(|t|t.context==context)else{monitor.stop();break;};
                            tab.info.dialog=Some(dialog);
                        },
                        Ok(Event::PromptClosed{context})=>{for tab in observed.lock().unwrap().values_mut().filter(|t|t.context==context){tab.info.dialog=None;}},
                        Ok(event)=>if !contexts.apply(&event){monitor.stop();break;},
                        Err(_)=>{monitor.stop();break;}
                    }
                }
            }
            decisions.abort_all();
        });
        let setup = async {
            channel
                .call(Command::SubscribePage, Policy::deadline())
                .await?;
            let reply = channel.call(Command::Intercept, Policy::deadline()).await?;
            if reply.value["intercept"].as_str().is_none_or(str::is_empty) {
                return Err(Error::Rejected(sem::types::decode()));
            }
            Ok(())
        }
        .await;
        if let Err(error) = setup {
            channel.stop();
            task.abort();
            owner.close(false).await.map_err(failure)?;
            return Err(failure(error));
        }
        let port = Semantic {
            channel,
            root,
            identities: Arc::new(Mutex::new(Identities::new())),
            coordination,
            tabs,
            policy: policy.clone(),
        };
        Ok(Self {
            port,
            owner,
            task,
            decisions,
        })
    }
    pub async fn navigate(&self, url: &str, deadline: Instant) -> Result<()> {
        self.port.navigate(url, deadline).await
    }
    pub async fn close(&mut self) -> Result<()> {
        self.port.coordination.close();
        self.owner.close(true).await.map_err(failure)?;
        self.task.abort();
        if !self.task.is_finished() {
            let _ = (&mut self.task).await;
        }
        Ok(())
    }
    pub fn policy_metrics(&self) -> sem::policy::Metrics {
        self.decisions.metrics()
    }
}

impl Semantic {
    pub(in crate::browser) async fn enable_task_events(
        &self,
        sink: crate::browser::semantic::task_events::Sink,
        _deadline: Instant,
    ) -> Result<()> {
        if sink.tracks_submit() {
            sink.set_request_context(self.root.as_str());
        }
        self.channel.set_task_events(self.root.clone(), Some(sink));
        Ok(())
    }

    pub fn tab_id(&self) -> Result<String> {
        self.tabs
            .lock()
            .unwrap()
            .values()
            .find(|t| t.context == self.root)
            .map(|t| t.info.tab_id.clone())
            .ok_or_else(|| Failure::new("browser_tab_unavailable"))
    }
    pub fn tabs(&self) -> Vec<Tab> {
        self.tabs
            .lock()
            .unwrap()
            .values()
            .map(|t| t.info.clone())
            .collect()
    }
    pub fn for_tab(&self, id: &str) -> Result<Self> {
        let root = self
            .tabs
            .lock()
            .unwrap()
            .get(id)
            .filter(|t| t.info.ready)
            .map(|t| t.context.clone())
            .ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
        Ok(Self {
            root,
            ..self.clone()
        })
    }
    pub fn dialog(&self) -> Option<Dialog> {
        self.tabs
            .lock()
            .unwrap()
            .values()
            .find(|t| t.context == self.root)
            .and_then(|t| t.info.dialog.clone())
    }
    pub async fn handle_dialog(&self, id: &str, accept: bool, deadline: Instant) -> Result<()> {
        if self.dialog().as_ref().is_none_or(|d| d.dialog_id != id) {
            return Err(Failure::new("browser_stale_frame"));
        }
        self.channel
            .call(
                Command::Dialog {
                    context: self.root.clone(),
                    accept,
                },
                deadline,
            )
            .await
            .map_err(failure)?;
        for tab in self
            .tabs
            .lock()
            .unwrap()
            .values_mut()
            .filter(|t| t.context == self.root)
        {
            if tab.info.dialog.as_ref().is_some_and(|d| d.dialog_id == id) {
                tab.info.dialog = None;
            }
        }
        Ok(())
    }
    pub async fn metadata(&self, deadline: Instant) -> Result<(sem::Frame, Metadata)> {
        if self.dialog().is_some() {
            return Err(Failure::new("browser_dialog_open"));
        }
        let frame = self
            .frames(deadline)
            .await?
            .into_iter()
            .next()
            .ok_or_else(sem::types::stale)?;
        let metadata = self.metadata_for(&frame, deadline).await?;
        Ok((frame, metadata))
    }
    pub async fn metadata_for(&self, frame: &sem::Frame, deadline: Instant) -> Result<Metadata> {
        if self.dialog().is_some() {
            return Err(Failure::new("browser_dialog_open"));
        }
        let Output::Surface(metadata) = self.program(frame, Program::Surface, deadline).await?
        else {
            return Err(sem::types::decode());
        };
        for tab in self
            .tabs
            .lock()
            .unwrap()
            .values_mut()
            .filter(|t| t.context == self.root)
        {
            tab.info.url = metadata.url.clone();
            tab.info.title = metadata.title.clone();
        }
        Ok(metadata)
    }
    pub async fn screenshot(&self, deadline: Instant) -> Result<Value> {
        let (_, metadata) = self.metadata(deadline).await?;
        let reply = self
            .channel
            .call(Command::Screenshot(self.root.clone()), deadline)
            .await
            .map_err(failure)?
            .value;
        let data = reply["data"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= crate::browser::image::MAX_ENCODED)
            .ok_or_else(|| Failure::new("browser_image_too_large"))?;
        Ok(
            json!({"data":data,"mime_type":"image/jpeg","width":metadata.width.round() as u64,"height":metadata.height.round() as u64,"css_pixel_scale":1,"scope":"viewport"}),
        )
    }
    pub async fn new_tab(&self, deadline: Instant) -> Result<String> {
        if self.tabs.lock().unwrap().len() >= 8 {
            return Err(Failure::new("browser_tab_limit"));
        }
        let reply = self
            .channel
            .call(Command::CreateTab, deadline)
            .await
            .map_err(failure)?
            .value;
        let context = self
            .channel
            .context(&reply["context"])
            .ok_or_else(sem::types::decode)?;
        loop {
            if let Some(id) = self
                .tabs
                .lock()
                .unwrap()
                .values()
                .find(|t| t.context == context)
                .map(|t| t.info.tab_id.clone())
            {
                return Ok(id);
            }
            if !self.connected() || Instant::now() >= deadline {
                return Err(Failure::unknown("browser_tab_unavailable"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn close_tab(&self, deadline: Instant) -> Result<()> {
        if self.tabs.lock().unwrap().len() <= 1 {
            return Err(Failure::new("browser_last_tab"));
        }
        self.channel
            .call(Command::CloseContext(self.root.clone()), deadline)
            .await
            .map_err(failure)?;
        self.tabs
            .lock()
            .unwrap()
            .retain(|_, t| t.context != self.root);
        Ok(())
    }
    pub async fn activate(&self, deadline: Instant) -> Result<()> {
        self.channel
            .call(Command::Activate(self.root.clone()), deadline)
            .await
            .map(drop)
            .map_err(failure)
    }
    pub async fn focused_frame(&self, frame: &sem::Frame, deadline: Instant) -> Result<bool> {
        let mut child = self.context(frame)?;
        let mut parent = frame.parent;
        let mut depth = 0;
        while let Some(key) = parent {
            depth += 1;
            if depth > sem::MAX_FRAMES {
                return Err(sem::types::decode());
            }
            let (context, next) = {
                let ids = self.identities.lock().unwrap();
                (
                    self.channel
                        .context(&json!(ids.key(key)?))
                        .ok_or_else(sem::types::stale)?,
                    ids.parent(key)?,
                )
            };
            let reply = self
                .channel
                .call(
                    Command::FrameOwner {
                        context: context.clone(),
                        child,
                    },
                    deadline,
                )
                .await
                .map_err(failure)?
                .value;
            let nodes = reply["nodes"].as_array().ok_or_else(sem::types::decode)?;
            if nodes.len() != 1 {
                return Err(sem::types::stale());
            }
            let node = nodes[0]["sharedId"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 512)
                .ok_or_else(sem::types::stale)?
                .to_owned();
            let checked = self
                .channel
                .call(
                    Command::FrameFocused {
                        context: context.clone(),
                        node,
                    },
                    deadline,
                )
                .await
                .map_err(failure)?
                .value;
            if !matches!(
                decode_program(&Program::NativeFrameFocused, checked)?,
                Output::Applied(Some(true))
            ) {
                return Ok(false);
            }
            child = context;
            parent = next;
        }
        Ok(child == self.root)
    }
    async fn viewport_target(
        &self,
        mut x: f64,
        mut y: f64,
        deadline: Instant,
    ) -> Result<(ContextId, f64, f64)> {
        let frames = self.frames(deadline).await?;
        let mut current = frames.first().ok_or_else(sem::types::stale)?;
        for _ in 0..sem::MAX_FRAMES {
            let context = self.context(current)?;
            if matches!(
                self.run(&context, Program::FrameAtPoint { x, y }, deadline)
                    .await?,
                Output::Applied(Some(false))
            ) {
                let after = self.frames(deadline).await?;
                if frames.len() != after.len() || !frames.iter().all(|f| after.contains(f)) {
                    return Err(Failure::new("browser_stale_frame"));
                }
                return Ok((context, x, y));
            }
            let mut next = None;
            for child in frames.iter().filter(|f| f.parent == Some(current.context)) {
                let child_context = self.context(child)?;
                let reply = self
                    .channel
                    .call(
                        Command::FrameOwner {
                            context: context.clone(),
                            child: child_context.clone(),
                        },
                        deadline,
                    )
                    .await
                    .map_err(failure)?
                    .value;
                let nodes = reply["nodes"].as_array().ok_or_else(sem::types::decode)?;
                if nodes.len() != 1 {
                    return Err(sem::types::stale());
                }
                let node = nodes[0]["sharedId"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .ok_or_else(sem::types::stale)?
                    .to_owned();
                let Output::Surface(meta) =
                    self.run(&child_context, Program::Surface, deadline).await?
                else {
                    return Err(sem::types::decode());
                };
                let geometry = sem::Geometry {
                    x,
                    y,
                    width: meta.width,
                    height: meta.height,
                    visible: true,
                    hit: true,
                    in_view: true,
                };
                let reply = self
                    .channel
                    .call(
                        Command::FramePoint {
                            context: context.clone(),
                            node,
                            geometry,
                        },
                        deadline,
                    )
                    .await
                    .map_err(failure)?
                    .value;
                match decode_program(&Program::NativeFramePoint, reply)? {
                    Output::Geometry(Some(point)) => {
                        if next.is_some() {
                            return Err(Failure::new("browser_stale_frame"));
                        }
                        next = Some((child, point.x, point.y));
                    }
                    Output::Geometry(None) => {}
                    _ => return Err(sem::types::decode()),
                }
            }
            let Some((child, child_x, child_y)) = next else {
                return Err(Failure::new("browser_geometry_unsupported"));
            };
            current = child;
            x = child_x;
            y = child_y;
        }
        Err(Failure::new("browser_frame_limit"))
    }
    async fn focused_context(&self, deadline: Instant) -> Result<ContextId> {
        let frames = self.frames(deadline).await?;
        let mut current = frames.first().ok_or_else(sem::types::stale)?;
        for _ in 0..sem::MAX_FRAMES {
            let mut next = None;
            for child in frames.iter().filter(|f| f.parent == Some(current.context)) {
                if self.focused_frame(child, deadline).await? {
                    if next.is_some() {
                        return Err(Failure::new("browser_stale_frame"));
                    }
                    next = Some(child);
                }
            }
            let Some(child) = next else {
                if self.refresh(current, deadline).await? != *current {
                    return Err(Failure::new("browser_stale_frame"));
                }
                return self.context(current);
            };
            current = child;
        }
        Err(Failure::new("browser_frame_limit"))
    }
    pub async fn visual_target_allowed(
        &self,
        frame: &sem::Frame,
        x: f64,
        y: f64,
        deadline: Instant,
    ) -> Result<bool> {
        let context = self.context(frame)?;
        let reply = self
            .channel
            .call(Command::VisualTarget { context, x, y }, deadline)
            .await
            .map_err(failure)?
            .value;
        if reply["type"] != "success" {
            return Err(Failure::new("browser_script_exception"));
        }
        let target = &reply["result"];
        if target["type"] != "node" {
            return Ok(false);
        }
        let node = &target["value"];
        Ok(node["localName"] == "canvas" || node["shadowRoot"]["value"]["mode"] == "closed")
    }

    pub async fn visual_hover(&self, x: f64, y: f64, deadline: Instant) -> Result<()> {
        self.action(&self.root, Action::Move { x, y }, deadline)
            .await
    }

    pub async fn visual_drag(
        &self,
        x: f64,
        y: f64,
        to_x: f64,
        to_y: f64,
        deadline: Instant,
    ) -> Result<()> {
        let (channel, effects) = self.channel.track_effects();
        let port = Self {
            channel,
            ..self.clone()
        };
        let context = &port.root;
        let result = async {
            port.action(context, Action::Move { x, y }, deadline)
                .await?;
            port.action(context, Action::Button { down: true }, deadline)
                .await?;
            for step in 1..=8 {
                let ratio = f64::from(step) / 8.0;
                port.action(
                    context,
                    Action::Move {
                        x: x + (to_x - x) * ratio,
                        y: y + (to_y - y) * ratio,
                    },
                    deadline,
                )
                .await?;
            }
            port.action(context, Action::Button { down: false }, deadline)
                .await
        }
        .await;
        if effects.load(std::sync::atomic::Ordering::Acquire) {
            let cleanup = port
                .channel
                .release_inputs(
                    context.clone(),
                    deadline.max(Instant::now() + Duration::from_secs(1)),
                )
                .await
                .map_err(failure);
            if cleanup.is_err() {
                port.channel.stop();
            }
            return result.and(cleanup.map(drop)).map_err(|mut error| {
                error.state = "unknown";
                error
            });
        }
        result
    }

    pub async fn visual_click(&self, x: f64, y: f64, deadline: Instant) -> Result<()> {
        self.human_input(HumanInput::Click { x, y }, deadline).await
    }
    pub async fn human_input(&self, input: HumanInput, deadline: Instant) -> Result<()> {
        let (channel, effects) = self.channel.track_effects();
        let port = Self {
            channel,
            ..self.clone()
        };
        let mut input_context = port.root.clone();
        let result = async {
            match input {
                HumanInput::Click { x, y } => {
                    let (context, x, y) = port.viewport_target(x, y, deadline).await?;
                    input_context = context;
                    port.action(&input_context, Action::Move { x, y }, deadline)
                        .await?;
                    port.action(&input_context, Action::Button { down: true }, deadline)
                        .await?;
                    port.action(&input_context, Action::Button { down: false }, deadline)
                        .await
                }
                HumanInput::Scroll { x, y, delta } => {
                    let (context, x, y) = port.viewport_target(x, y, deadline).await?;
                    input_context = context;
                    port.action(&input_context, Action::Wheel { x, y, delta }, deadline)
                        .await
                }
                HumanInput::Key(key) => {
                    input_context = port.focused_context(deadline).await?;
                    port.key(&input_context, key.bidi(), deadline).await
                }
                HumanInput::Text { frame, focus, text } => {
                    if port.refresh(&frame, deadline).await? != *frame {
                        return Err(Failure::new("browser_stale_frame"));
                    }
                    if !port.focused_frame(&frame, deadline).await? {
                        return Err(Failure::new("browser_stale_frame"));
                    }
                    let context = port.context(&frame)?;
                    input_context = context.clone();
                    let Output::Surface(metadata) =
                        port.run(&context, Program::Surface, deadline).await?
                    else {
                        return Err(sem::types::decode());
                    };
                    if metadata.focus.as_ref() != Some(&focus) {
                        return Err(Failure::new("browser_stale_frame"));
                    }
                    for chunk in text.chars().collect::<Vec<_>>().chunks(64) {
                        port.action(&context, Action::Text(chunk.iter().collect()), deadline)
                            .await?;
                    }
                    Ok(())
                }
            }
        }
        .await;
        if effects.load(std::sync::atomic::Ordering::Acquire) {
            let cleanup = port
                .channel
                .release_inputs(
                    input_context,
                    deadline.max(Instant::now() + Duration::from_secs(1)),
                )
                .await
                .map_err(failure);
            if cleanup.is_err() {
                self.channel.stop();
            }
            result.and(cleanup.map(drop)).map_err(|mut error| {
                error.state = "unknown";
                error
            })
        } else {
            result
        }
    }
    pub async fn navigate(&self, url: &str, deadline: Instant) -> Result<()> {
        if !self.policy.allows(url, deadline).await {
            return Err(Failure::new("browser_site_denied"));
        }
        self.channel
            .call(
                Command::Navigate {
                    context: self.root.clone(),
                    url: url.into(),
                },
                deadline,
            )
            .await
            .map(drop)
            .map_err(failure)
    }
    #[cfg(test)]
    pub fn advance_semantic_realm(&self) {
        self.channel.advance_semantic_realm();
    }
    #[cfg(test)]
    pub fn test_metrics(&self) -> Value {
        self.channel.snapshot()
    }
    #[cfg(test)]
    pub fn hold_next_reply(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.channel.hold_next_reply()
    }
    fn context(&self, frame: &sem::Frame) -> Result<ContextId> {
        let key = self
            .identities
            .lock()
            .unwrap()
            .key(frame.context)?
            .to_owned();
        self.channel
            .context(&json!(key))
            .ok_or_else(sem::types::stale)
    }
    async fn run(
        &self,
        context: &ContextId,
        program: Program,
        deadline: Instant,
    ) -> Result<Output> {
        let reply = self
            .channel
            .call(
                Command::Program {
                    context: context.clone(),
                    program: program.clone(),
                },
                deadline,
            )
            .await
            .map_err(failure)?;
        decode_program(&program, reply.value)
    }
    async fn stamp(
        &self,
        context: &ContextId,
        parent: Option<sem::Context>,
        deadline: Instant,
    ) -> Result<sem::Frame> {
        let navigation = self.channel.navigation(context).map_err(failure)?;
        let document = self
            .channel
            .call(Command::Document(context.clone()), deadline)
            .await
            .map_err(failure)?
            .value;
        if document["type"] == "exception" {
            return Err(Failure::new("browser_script_exception"));
        }
        if document["type"] != "success" || document["result"]["type"] != "node" {
            return Err(sem::types::decode());
        }
        let key = document["result"]["sharedId"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 512)
            .ok_or_else(sem::types::decode)?;
        let realm = document["realm"]
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 512)
            .ok_or_else(sem::types::decode)?;
        let Output::Marker(marker) = self.run(context, Program::Stamp, deadline).await? else {
            return Err(sem::types::decode());
        };
        if navigation != self.channel.navigation(context).map_err(failure)? {
            return Err(sem::types::stale());
        }
        let document_key = format!("{key}/{}", navigation.as_deref().unwrap_or("initial"));
        self.identities.lock().unwrap().observe(
            context.as_str(),
            parent,
            &document_key,
            realm,
            marker,
        )
    }
    async fn action(&self, context: &ContextId, action: Action, deadline: Instant) -> Result<()> {
        self.channel
            .call(
                Command::Action {
                    context: context.clone(),
                    action,
                },
                deadline,
            )
            .await
            .map(drop)
            .map_err(failure)
    }
    async fn key(&self, context: &ContextId, value: &str, deadline: Instant) -> Result<()> {
        self.action(
            context,
            Action::Key {
                value: value.into(),
                down: true,
            },
            deadline,
        )
        .await?;
        self.action(
            context,
            Action::Key {
                value: value.into(),
                down: false,
            },
            deadline,
        )
        .await
    }
    #[cfg(test)]
    pub async fn fixture(&self, expression: &str, deadline: Instant) -> Result<Value> {
        let reply = self
            .channel
            .call(
                Command::Fixture {
                    context: self.root.clone(),
                    expression: format!("JSON.stringify({expression})"),
                },
                deadline,
            )
            .await
            .map_err(failure)?
            .value;
        if reply["type"] != "success" || reply["result"]["type"] != "string" {
            return Err(Failure::new("browser_fixture_failed"));
        }
        serde_json::from_str(
            reply["result"]["value"]
                .as_str()
                .ok_or_else(sem::types::decode)?,
        )
        .map_err(|_| sem::types::decode())
    }
}
impl Port for Semantic {
    fn coordination(&self) -> Arc<sem::Coordination> {
        self.coordination.clone()
    }
    fn authorize(&self, control: Arc<Control>, epoch: u64, human: bool) -> Self {
        Self {
            channel: self.channel.authorized(control, epoch, human),
            ..self.clone()
        }
    }
    fn tracked(&self) -> (Self, Arc<AtomicBool>) {
        let (channel, flag) = self.channel.track_effects();
        (
            Self {
                channel,
                ..self.clone()
            },
            flag,
        )
    }
    fn connected(&self) -> bool {
        self.channel.connected()
    }
    fn stop(&self) {
        self.channel.stop()
    }
    fn pending(&self) -> usize {
        self.channel.pending_effects()
    }
    async fn frames(&self, deadline: Instant) -> Result<Vec<sem::Frame>> {
        let tree = self
            .channel
            .call(Command::Tree, deadline)
            .await
            .map_err(failure)?
            .value;
        let values = tree["contexts"].as_array().ok_or_else(sem::types::decode)?;
        let root = values
            .iter()
            .find(|v| v["context"] == self.root.as_str())
            .ok_or_else(sem::types::stale)?;
        let mut todo = std::collections::VecDeque::from([(root, None::<String>)]);
        let mut native = Vec::new();
        let mut keys = Vec::new();
        while let Some((node, parent)) = todo.pop_front() {
            if native.len() >= sem::types::MAX_FRAMES {
                return Err(Failure::new("browser_frame_limit"));
            }
            let context = self
                .channel
                .context(&node["context"])
                .ok_or_else(sem::types::decode)?;
            keys.push(context.as_str().to_owned());
            if let Some(children) = node["children"].as_array() {
                for child in children {
                    todo.push_back((child, Some(context.as_str().to_owned())));
                }
            }
            native.push((context, parent));
        }
        self.identities.lock().unwrap().retain(&keys);
        let mut output = Vec::new();
        for (context, parent) in native {
            let parent = parent
                .map(|key| self.identities.lock().unwrap().context(&key))
                .transpose()?;
            output.push(self.stamp(&context, parent, deadline).await?);
        }
        Ok(output)
    }
    async fn refresh(&self, frame: &sem::Frame, deadline: Instant) -> Result<sem::Frame> {
        self.stamp(&self.context(frame)?, frame.parent, deadline)
            .await
    }
    async fn target(
        &self,
        frame: &sem::Frame,
        node: u64,
        deadline: Instant,
    ) -> Result<sem::Geometry> {
        let program = Program::Geometry {
            marker: frame.marker.clone(),
            node,
        };
        let mut child = self.context(frame)?;
        let Output::Geometry(Some(mut geometry)) =
            self.run(&child, program.clone(), deadline).await?
        else {
            return Err(sem::types::stale());
        };
        let mut parent = frame.parent;
        let mut depth = 0;
        while let Some(key) = parent {
            depth += 1;
            if depth > sem::MAX_FRAMES {
                return Err(sem::types::decode());
            }
            let (context, next) = {
                let ids = self.identities.lock().unwrap();
                (
                    self.channel
                        .context(&json!(ids.key(key)?))
                        .ok_or_else(sem::types::stale)?,
                    ids.parent(key)?,
                )
            };
            let reply = self
                .channel
                .call(
                    Command::FrameOwner {
                        context: context.clone(),
                        child,
                    },
                    deadline,
                )
                .await
                .map_err(failure)?
                .value;
            let nodes = reply["nodes"].as_array().ok_or_else(sem::types::decode)?;
            if nodes.len() != 1 {
                return Err(sem::types::stale());
            }
            let node = nodes[0]["sharedId"]
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= 512)
                .ok_or_else(sem::types::stale)?
                .to_owned();
            let mapped = self
                .channel
                .call(
                    Command::FrameGeometry {
                        context: context.clone(),
                        node,
                        geometry,
                    },
                    deadline,
                )
                .await
                .map_err(failure)?
                .value;
            let Output::Geometry(Some(mapped)) = decode_program(&program, mapped)? else {
                return Err(Failure::new("browser_geometry_unsupported"));
            };
            geometry = mapped;
            child = context;
            parent = next;
        }
        Ok(geometry)
    }
    async fn program(
        &self,
        frame: &sem::Frame,
        program: Program,
        deadline: Instant,
    ) -> Result<Output> {
        self.run(&self.context(frame)?, program, deadline).await
    }
    async fn input(
        &self,
        frame: &sem::Frame,
        input: sem::NativeInput,
        deadline: Instant,
    ) -> Result<()> {
        let context = self.context(frame)?;
        let (channel, effects) = self.channel.track_effects();
        let owned = Self {
            channel,
            ..self.clone()
        };
        let result = async {
            let hover = matches!(input, sem::NativeInput::Hover { .. });
            let reuse_radio = match &input {
                sem::NativeInput::Click { reuse_radio, .. } => *reuse_radio,
                _ => None,
            };
            match input {
                sem::NativeInput::Drag { .. } => Err(Failure::new("browser_feature_unsupported")),
                sem::NativeInput::SelectFile { node, path } => {
                    let reply = owned
                        .channel
                        .call(
                            Command::Node {
                                context: context.clone(),
                                marker: frame.marker.clone(),
                                node,
                            },
                            deadline,
                        )
                        .await
                        .map_err(failure)?
                        .value;
                    if reply["type"] != "success" || reply["result"]["type"] != "node" {
                        return Err(sem::types::stale());
                    }
                    let node = reply["result"]["sharedId"]
                        .as_str()
                        .filter(|v| !v.is_empty() && v.len() <= 512)
                        .ok_or_else(sem::types::stale)?
                        .to_owned();
                    owned
                        .channel
                        .call(
                            Command::SelectFile {
                                context: context.clone(),
                                node,
                                path,
                            },
                            deadline,
                        )
                        .await
                        .map_err(failure)
                        .map(drop)
                }
                sem::NativeInput::Click { manner, .. } if manner != sem::types::Manner::Once => {
                    Err(Failure::new("browser_feature_unsupported"))
                }
                sem::NativeInput::Hover { node } | sem::NativeInput::Click { node, .. } => {
                    let submit_node = node;
                    let geometry = owned.target(frame, node, deadline).await?;
                    if !geometry.visible || !geometry.hit || !geometry.in_view {
                        return Err(Failure::new("browser_not_actionable"));
                    }
                    let reply = owned
                        .channel
                        .call(
                            Command::Node {
                                context: context.clone(),
                                marker: frame.marker.clone(),
                                node,
                            },
                            deadline,
                        )
                        .await
                        .map_err(failure)?
                        .value;
                    if reply["type"] != "success" || reply["result"]["type"] != "node" {
                        return Err(sem::types::stale());
                    }
                    let node = reply["result"]["sharedId"]
                        .as_str()
                        .filter(|v| !v.is_empty() && v.len() <= 512)
                        .ok_or_else(sem::types::stale)?
                        .to_owned();
                    owned
                        .action(&context, Action::MoveTo { node }, deadline)
                        .await?;
                    if hover {
                        return Ok(());
                    }
                    #[cfg(test)]
                    self.coordination.before_click_dispatch().await;
                    if let Some(radio) = reuse_radio {
                        let now = owned.target(frame, submit_node, deadline).await?;
                        if !now.hit
                            || !now.visible
                            || !now.in_view
                            || now.x != geometry.x
                            || now.y != geometry.y
                        {
                            return Err(sem::types::stale());
                        }
                        sem::program::verify_reusable_submit(
                            &owned,
                            frame,
                            radio,
                            submit_node,
                            deadline,
                        )
                        .await?;
                    }
                    owned
                        .action(&context, Action::Button { down: true }, deadline)
                        .await?;
                    owned
                        .action(&context, Action::Button { down: false }, deadline)
                        .await
                }
                sem::NativeInput::ReplaceText(text) => {
                    owned.action(&context, Action::SelectAll, deadline).await?;
                    owned.key(&context, "\u{e003}", deadline).await?;
                    let chars = text.chars().collect::<Vec<_>>();
                    for chunk in chars.chunks(64) {
                        owned
                            .action(&context, Action::Text(chunk.iter().collect()), deadline)
                            .await?;
                    }
                    Ok(())
                }
                sem::NativeInput::Press(key) => {
                    let value = match key {
                        sem::Key::Enter => "\u{e007}",
                        sem::Key::Tab => "\u{e004}",
                        sem::Key::Escape => "\u{e00c}",
                        sem::Key::Backspace => "\u{e003}",
                        sem::Key::ArrowDown => "\u{e015}",
                        sem::Key::ArrowUp => "\u{e013}",
                        sem::Key::ArrowLeft => "\u{e012}",
                        sem::Key::ArrowRight => "\u{e014}",
                    };
                    owned.key(&context, value, deadline).await
                }
            }
        }
        .await;
        if effects.load(std::sync::atomic::Ordering::Acquire) {
            let released = owned
                .channel
                .release_inputs(
                    context,
                    deadline.max(Instant::now() + Duration::from_secs(1)),
                )
                .await
                .map_err(failure);
            if released.is_err() {
                self.channel.stop();
            }
            result.and(released.map(drop)).map_err(|mut error| {
                error.state = "unknown";
                error
            })
        } else {
            result
        }
    }
}
fn decode_program(program: &Program, reply: Value) -> Result<Output> {
    if reply["type"] == "exception" {
        return Err(Failure::new("browser_script_exception"));
    }
    if reply["type"] != "success" || reply["result"]["type"] != "string" {
        return Err(sem::types::decode());
    }
    program.decode(
        reply["result"]["value"]
            .as_str()
            .ok_or_else(sem::types::decode)?,
    )
}
pub(super) fn failure(error: Error) -> Failure {
    match error {
        Error::Rejected(failure) => failure,
        Error::Protocol(ProtocolError::NoSuchFrame) => sem::types::stale(),
        Error::Protocol(ProtocolError::UnknownCommand | ProtocolError::Unsupported) => {
            Failure::new("browser_unsupported")
        }
        Error::Protocol(_) => Failure::new("browser_command_failed"),
        Error::TimeoutUnknown => Failure::unknown("browser_timeout_unknown"),
        Error::DeliveryRevoked => Failure::new("browser_control_changed"),
        Error::CleanupPending => Failure::unknown("browser_cleanup_pending"),
        Error::Transport(_) => Failure::unknown("browser_lost"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exceptions_missing_null_and_nonfinite_values_are_not_empty_observations() {
        for reply in [
            json!({"type":"exception","exceptionDetails":{}}),
            json!({"type":"success","result":{"type":"undefined"}}),
            json!({"type":"success","result":{"type":"null"}}),
            json!({"type":"success","result":{"type":"number","value":"NaN"}}),
            json!({"type":"success","result":{"type":"string","value":"{}"}}),
        ] {
            assert!(decode_program(&Program::Stamp, reply).is_err());
        }
    }
}
