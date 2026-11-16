//! CDP encoding for the explicit semantic_dom_v1 profile.
#![allow(dead_code)] // Internal profile; capability v2 selects it in D4.
use super::*;
use crate::browser::semantic::{
    self as sem, Port,
    policy::{Capabilities, Decisions, Policy},
    program::{Output, Program},
    types::Identities,
};
use sem::surface::{Dialog, HumanInput, Metadata, Tab};
use std::{collections::HashMap, sync::Mutex};

#[derive(Clone)]
struct NativeTab {
    info: Tab,
    context: Context,
    native: TabId,
}

#[derive(Clone)]
pub(in crate::browser) struct Semantic {
    adapter: Adapter,
    root: Context,
    identity: Arc<Mutex<Identities>>,
    routes: Arc<Mutex<HashMap<String, Frame>>>,
    coordination: Arc<sem::Coordination>,
    tabs: Arc<Mutex<HashMap<String, NativeTab>>>,
    policy: Policy,
    #[cfg(test)]
    realm_variant: Arc<std::sync::atomic::AtomicU64>,
}
pub(in crate::browser) struct Session {
    pub port: Semantic,
    engine: Engine,
    task: tokio::task::JoinHandle<()>,
    owner: Option<crate::browser::process::BrowserProcess>,
    decisions: Decisions,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.port.coordination.close();
        self.engine.stop();
        self.task.abort();
    }
}
impl Session {
    pub async fn launch(
        browser: &std::path::Path,
        root: &std::path::Path,
        policy: Policy,
        deadline: Instant,
    ) -> Result<Self> {
        Self::launch_with(
            browser,
            root,
            "profile",
            policy,
            Arc::new(sem::Coordination::new()),
            deadline,
        )
        .await
    }
    pub async fn launch_with(
        browser: &std::path::Path,
        root: &std::path::Path,
        id: &str,
        policy: Policy,
        coordination: Arc<sem::Coordination>,
        deadline: Instant,
    ) -> Result<Self> {
        policy.admit(Capabilities {
            navigation_intercept: true,
            resolved_destination_check: true,
            observed_connection_address: false,
        })?;
        let mut owner =
            crate::browser::process::BrowserProcess::launch(browser, root, id, None, deadline)
                .await?;
        match Self::attach(owner.engine.clone(), policy, coordination, deadline).await {
            Ok(mut session) => {
                session.owner = Some(owner);
                Ok(session)
            }
            Err(error) => {
                owner.close().await?;
                Err(error)
            }
        }
    }
    pub async fn close(&mut self) -> Result<()> {
        self.port.coordination.close();
        if let Some(owner) = &mut self.owner {
            owner.close().await?;
        }
        self.task.abort();
        if !self.task.is_finished() {
            let _ = (&mut self.task).await;
        }
        Ok(())
    }
    /// The private attach stage installs the global barrier before exposing a
    /// page. Policy admission has already preceded native process allocation.
    async fn attach(
        engine: Engine,
        policy: Policy,
        coordination: Arc<sem::Coordination>,
        deadline: Instant,
    ) -> Result<Self> {
        policy.admit(Capabilities {
            navigation_intercept: true,
            resolved_destination_check: true,
            observed_connection_address: false,
        })?;
        let mut events = engine.events();
        let (first, ready) = tokio::sync::oneshot::channel();
        let monitor = engine.clone();
        let rules = policy.clone();
        let decisions = Decisions::default();
        let ledger = decisions.clone();
        let tabs = Arc::new(Mutex::new(HashMap::<String, NativeTab>::new()));
        let observed = tabs.clone();
        let task = tokio::spawn(async move {
            let mut first = Some(first);
            let mut decisions: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
            let mut initializing: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    completed=decisions.join_next(),if !decisions.is_empty()=>{
                        if !matches!(completed,Some(Ok(Ok(())))) {monitor.stop();break;}
                    },
                    completed=initializing.join_next(),if !initializing.is_empty()=>{
                        if !matches!(completed,Some(Ok(Ok(())))) {monitor.stop();break;}
                    },
                    event=events.recv()=>match event {
                        Ok(Event::Attached{context,info})=>{
                            ledger.context_created();
                            if !matches!(info.kind,ContextKind::Page|ContextKind::Frame) {
                                if monitor.discard_tab(&info.tab,Policy::deadline()).await.is_err(){monitor.stop();break;}
                                continue;
                            }
                            // Enabling domains on an initial popup can wait for
                            // its intercepted document request. Keep draining
                            // and deciding requests while that setup is pending.
                            if initializing.len()>=32 {monitor.stop();break;}
                            let tab = if info.kind==ContextKind::Page {
                                if observed.lock().unwrap().len() >= 8 {
                                    if monitor.discard_tab(&info.tab,Policy::deadline()).await.is_err(){monitor.stop();break;}
                                    continue;
                                }
                                let id=match crate::random::hex_token::<16>() {Ok(id)=>id,Err(_)=>{monitor.stop();break;}};
                                observed.lock().unwrap().insert(id.clone(),NativeTab{info:Tab{tab_id:id.clone(),title:info.title.clone(),url:info.url.clone(),ready:false,dialog:None},context:context.clone(),native:info.tab.clone()});
                                Some(id)
                            } else {None};
                            let port=monitor.clone();let all=observed.clone();
                            let ready=if info.kind==ContextKind::Page{first.take()}else{None};
                            initializing.spawn(async move {
                                port.initialize_context(&context,false,false,Instant::now()+std::time::Duration::from_secs(10)).await?;
                                if let Some(id)=tab && let Some(tab)=all.lock().unwrap().get_mut(&id){tab.info.ready=true;}
                                if let Some(ready)=ready {let _=ready.send(context);}
                                Ok(())
                            });
                        },
                        Ok(Event::DocumentPaused(request))=>{
                            if decisions.len()>=32 {monitor.stop();break;}
                            // Fetch IDs identify an individual redirect hop;
                            // the network ID can be reused across those hops.
                            let context=request.context.as_ref().and_then(|context|monitor.backend.session(context).ok()).unwrap_or("");
                            let binding=format!("{context}/{}",request.frame.v1());
                            let token=match ledger.claim(&binding,&request.handle.key,u32::from(request.redirected),&request.url,true,!context.is_empty()&&!request.frame.v1().is_empty()){Ok(token)=>token,Err(_)=>{monitor.stop();break;}};
                            let port=monitor.clone();let rules=rules.clone();
                            decisions.spawn(async move {
                                let allowed=token.allows(&rules).await;
                                port.respond(&request.handle,if allowed{RequestDecision::Continue}else{RequestDecision::Deny},token.deadline()).await?;token.settled(allowed);Ok(())
                            });
                        },
                        Ok(Event::Detached(context))=>{observed.lock().unwrap().retain(|_,tab|tab.context!=context);},
                        Ok(Event::TargetChanged(info))=>{for tab in observed.lock().unwrap().values_mut().filter(|t|t.native==info.tab){tab.info.title=info.title.chars().take(4096).collect();tab.info.url=info.url.chars().take(8192).collect();}},
                        Ok(Event::DialogOpened{context,kind,message})=>{
                            let dialog=match Dialog::new(kind.as_str().unwrap_or(""),&message){Ok(d)=>d,Err(_)=>{monitor.stop();break;}};
                            let mut all=observed.lock().unwrap();
                            let Some(tab)=all.values_mut().find(|t|Some(&t.context)==context.as_ref())else{monitor.stop();break;};
                            tab.info.dialog=Some(dialog);
                        },
                        Ok(Event::DialogClosed{context})=>{for tab in observed.lock().unwrap().values_mut().filter(|t|Some(&t.context)==context.as_ref()){tab.info.dialog=None;}},
                        Ok(_)=>{},Err(_)=>{monitor.stop();break;}
                    }
                }
            }
            decisions.abort_all();
            initializing.abort_all();
        });
        let setup = async {
            engine.initialize(deadline).await?;
            tokio::time::timeout_at(deadline.into(), ready)
                .await
                .map_err(|_| Failure::new("browser_start_failed"))?
                .map_err(|_| Failure::new("browser_start_failed"))
        }
        .await;
        let root = match setup {
            Ok(root) => root,
            Err(error) => {
                engine.stop();
                task.abort();
                return Err(error);
            }
        };
        let port = Semantic {
            adapter: engine.backend.clone(),
            root,
            identity: Arc::new(Mutex::new(Identities::new())),
            routes: Arc::new(Mutex::new(HashMap::new())),
            coordination,
            tabs,
            policy: policy.clone(),
            #[cfg(test)]
            realm_variant: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        Ok(Self {
            port,
            engine,
            task,
            owner: None,
            decisions,
        })
    }
    pub async fn navigate(&self, url: &str, deadline: Instant) -> Result<()> {
        self.port.navigate(url, deadline).await
    }
    pub fn policy_metrics(&self) -> sem::policy::Metrics {
        self.decisions.metrics()
    }
}
impl Semantic {
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
        self.engine().dialog(&self.root, accept, deadline).await?;
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
            .ok_or_else(stale)?;
        let Output::Surface(metadata) = self.program(&frame, Program::Surface, deadline).await?
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
            tab.info.title = metadata.title.clone();
            tab.info.url = metadata.url.clone();
        }
        Ok((frame, metadata))
    }
    pub async fn screenshot(&self, deadline: Instant) -> Result<Value> {
        crate::browser::image::capture(&self.engine(), &self.root, deadline).await
    }
    pub async fn new_tab(&self, deadline: Instant) -> Result<String> {
        if self.tabs.lock().unwrap().len() >= 8 {
            return Err(Failure::new("browser_tab_limit"));
        }
        let native = self.engine().new_tab(deadline).await?;
        loop {
            if let Some(id) = self
                .tabs
                .lock()
                .unwrap()
                .values()
                .find(|t| t.native == native && t.info.ready)
                .map(|t| t.info.tab_id.clone())
            {
                return Ok(id);
            }
            if !self.connected() || Instant::now() >= deadline {
                return Err(Failure::unknown("browser_tab_unavailable"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    pub async fn close_tab(&self, deadline: Instant) -> Result<()> {
        let native = {
            let all = self.tabs.lock().unwrap();
            if all.len() <= 1 {
                return Err(Failure::new("browser_last_tab"));
            }
            all.values()
                .find(|t| t.context == self.root)
                .map(|t| t.native.clone())
                .ok_or_else(stale)?
        };
        self.engine().close_tab(&native, deadline).await?;
        self.tabs
            .lock()
            .unwrap()
            .retain(|_, t| t.context != self.root);
        Ok(())
    }
    pub async fn activate(&self, deadline: Instant) -> Result<()> {
        self.engine().activate(&self.root, deadline).await
    }
    pub async fn focused_frame(&self, frame: &sem::Frame, deadline: Instant) -> Result<bool> {
        let mut child = self.native(frame)?;
        let mut parent = frame.parent;
        let mut depth = 0;
        while let Some(context) = parent {
            depth += 1;
            if depth > sem::MAX_FRAMES {
                return Err(sem::types::decode());
            }
            let (key, next) = {
                let ids = self.identity.lock().unwrap();
                (ids.key(context)?.to_owned(), ids.parent(context)?)
            };
            let native = self
                .routes
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .ok_or_else(stale)?;
            let owner = self
                .engine()
                .frame_owner(&native.session, &child.id, deadline)
                .await?;
            let world = self.world(&native, deadline).await?;
            let session = Some(self.adapter.session(&native.session)?);
            let reply = self.adapter.wire.call(session, Command::DomResolveNode,
                json!({"backendNodeId":self.adapter.node(&owner)?,"executionContextId":world,"objectGroup":"rdh-semantic-focus"}),deadline).await?;
            let object = reply["object"]["objectId"].as_str().ok_or_else(stale)?;
            let checked = self.adapter.wire.call(session,Command::RuntimeCallFunction(Effect::None),
                json!({"executionContextId":world,"functionDeclaration":sem::program::SOURCE,"arguments":[{"value":"native_focused_frame"},{"value":null},{"objectId":object}],"returnByValue":true}),deadline).await;
            let released = self
                .adapter
                .wire
                .cleanup()
                .call(
                    session,
                    Command::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    deadline.max(Instant::now() + std::time::Duration::from_secs(1)),
                )
                .await;
            if released.is_err() {
                self.adapter.stop();
                return Err(Failure::unknown("browser_cleanup_pending"));
            }
            if !matches!(
                decode_program(&Program::NativeFrameFocused, checked?)?,
                Output::Applied(Some(true))
            ) {
                return Ok(false);
            }
            child = native;
            parent = next;
        }
        Ok(child.session == self.root)
    }
    pub async fn human_input(&self, input: HumanInput, deadline: Instant) -> Result<()> {
        let engine = self.engine();
        match input {
            HumanInput::Click { x, y } => engine.click(&self.root, Point { x, y }, deadline).await,
            HumanInput::Scroll { x, y, delta } => {
                engine
                    .wheel(&self.root, Point { x, y }, delta, deadline)
                    .await
            }
            HumanInput::Key(key) => engine.press(&self.root, key.name(), deadline).await,
            HumanInput::Text { frame, focus, text } => {
                let current = self.refresh(&frame, deadline).await?;
                if current != *frame {
                    return Err(Failure::new("browser_stale_frame"));
                }
                if !self.focused_frame(&frame, deadline).await? {
                    return Err(Failure::new("browser_stale_frame"));
                }
                let Output::Surface(metadata) =
                    self.program(&frame, Program::Surface, deadline).await?
                else {
                    return Err(sem::types::decode());
                };
                if metadata.focus.as_ref() != Some(&focus) {
                    return Err(Failure::new("browser_stale_frame"));
                }
                let native = self.native(&frame)?;
                self.adapter
                    .wire
                    .call(
                        Some(self.adapter.session(&native.session)?),
                        Command::InputInsertText,
                        json!({"text":text}),
                        deadline,
                    )
                    .await
                    .map(drop)
            }
        }
    }
    pub async fn navigate(&self, url: &str, deadline: Instant) -> Result<()> {
        if !self.policy.allows(url, deadline).await {
            return Err(Failure::new("browser_site_denied"));
        }
        let reply = self
            .adapter
            .wire
            .call(
                Some(self.adapter.session(&self.root)?),
                Command::PageNavigate,
                json!({"url":url}),
                deadline,
            )
            .await?;
        if reply.get("errorText").is_some() {
            return Err(Failure::unknown("browser_navigation_failed"));
        }
        loop {
            let native = self.engine().page_frames(&self.root, deadline).await?;
            if let Some(top) = native.first() {
                let document = self.adapter.key(&top.document.0)?;
                if reply["loaderId"]
                    .as_str()
                    .is_none_or(|expected| expected == document)
                {
                    let world = self.world(top, deadline).await?;
                    if matches!(
                        self.run(top, world, &Program::Ready, deadline).await?,
                        Output::Applied(Some(true))
                    ) {
                        return Ok(());
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(Failure::unknown("browser_navigation_timeout"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    #[cfg(test)]
    pub fn advance_semantic_realm(&self) {
        self.realm_variant
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
    #[cfg(test)]
    pub fn test_metrics(&self) -> Value {
        self.adapter.wire.test_metrics()
    }
    #[cfg(test)]
    pub fn hold_next_reply(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.adapter.wire.hold_next_reply()
    }
    async fn leased_pair(
        &self,
        session: &str,
        pair: InputPair,
        press: Value,
        release: Value,
        deadline: Instant,
    ) -> Result<()> {
        let (down, up) = match pair {
            InputPair::Key => (Command::InputKeyPress, Command::InputKeyRelease),
            InputPair::Mouse => (Command::InputMousePress, Command::InputMouseRelease),
        };
        let pressed = self
            .adapter
            .wire
            .call(Some(session), down, press, deadline)
            .await;
        if pressed
            .as_ref()
            .is_err_and(|error| error.state == "not_sent")
        {
            return pressed.map(drop);
        }
        let released = self
            .adapter
            .wire
            .cleanup()
            .call(
                Some(session),
                up,
                release,
                deadline.max(Instant::now() + std::time::Duration::from_secs(1)),
            )
            .await;
        // A dialog can defer the native release reply. Wire retains that
        // command and its input barrier; explicit dialog recovery can settle
        // it. Other cleanup failures still stop the connection.
        if released
            .as_ref()
            .is_err_and(|e| e.code != "browser_dialog_open")
        {
            self.adapter.stop();
        }
        pressed.and(released).map(drop).map_err(|mut error| {
            error.state = "unknown";
            error
        })
    }
    fn engine(&self) -> Engine {
        Engine {
            backend: self.adapter.clone(),
        }
    }
    fn native(&self, frame: &sem::Frame) -> Result<Frame> {
        let key = self.identity.lock().unwrap().key(frame.context)?.to_owned();
        self.routes
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(stale)
    }
    async fn native_node(
        &self,
        frame: &Frame,
        marker: &sem::Marker,
        node: u64,
        deadline: Instant,
    ) -> Result<Node> {
        let world = self.world(frame, deadline).await?;
        let session = Some(self.adapter.session(&frame.session)?);
        let reply=self.adapter.wire.call(session,Command::RuntimeCallFunction(Effect::None),json!({"executionContextId":world,"functionDeclaration":sem::program::NATIVE_NODE,"arguments":[{"value":json!({"realm":marker.realm,"restoration":marker.restoration,"node":node}).to_string()}],"objectGroup":"rdh-semantic-input","returnByValue":false}),deadline).await?;
        if reply.get("exceptionDetails").is_some() {
            return Err(Failure::new("browser_script_exception"));
        }
        let object = reply["result"]["objectId"].as_str().ok_or_else(stale)?;
        let described = self
            .adapter
            .wire
            .call(
                session,
                Command::DomDescribeNode,
                json!({"objectId":object,"depth":0}),
                deadline,
            )
            .await;
        let released = self
            .adapter
            .wire
            .cleanup()
            .call(
                session,
                Command::RuntimeReleaseObject,
                json!({"objectId":object}),
                deadline.max(Instant::now() + std::time::Duration::from_secs(1)),
            )
            .await;
        if released.is_err() {
            self.adapter.stop();
            return Err(Failure::unknown("browser_cleanup_pending"));
        }
        let key = described?["node"]["backendNodeId"]
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or_else(stale)?;
        Ok(Node {
            context: frame.session.clone(),
            key,
        })
    }
    async fn point(
        &self,
        frame: &Frame,
        node: &Node,
        marker: &sem::Marker,
        token: u64,
        deadline: Instant,
    ) -> Result<Point> {
        let engine = self.engine();
        let quad = engine.quad(node, deadline).await?;
        let point = Point {
            x: (quad[0] + quad[2] + quad[4] + quad[6]) / 4.,
            y: (quad[1] + quad[3] + quad[5] + quad[7]) / 4.,
        };
        let hit = engine.hit_at(&frame.session, point, deadline).await?;
        if hit.frame != frame.id {
            return Err(actionable());
        }
        if hit.node != *node {
            let world = self.world(frame, deadline).await?;
            let session = Some(self.adapter.session(&frame.session)?);
            let object=self.adapter.wire.call(session,Command::DomResolveNode,json!({"backendNodeId":self.adapter.node(&hit.node)?,"executionContextId":world,"objectGroup":"rdh-semantic-hit"}),deadline).await?;
            let object = object["object"]["objectId"].as_str().ok_or_else(stale)?;
            let checked=self.adapter.wire.call(session,Command::RuntimeCallFunction(Effect::None),json!({"executionContextId":world,"functionDeclaration":sem::program::SOURCE,"arguments":[{"value":"native_hit"},{"value":{"realm":marker.realm,"restoration":marker.restoration,"node":token}},{"objectId":object}],"returnByValue":true}),deadline).await;
            let released = self
                .adapter
                .wire
                .cleanup()
                .call(
                    session,
                    Command::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    deadline.max(Instant::now() + std::time::Duration::from_secs(1)),
                )
                .await;
            if released.is_err() {
                self.adapter.stop();
                return Err(Failure::unknown("browser_cleanup_pending"));
            }
            let checked = checked?;
            if checked.get("exceptionDetails").is_some()
                || checked["result"]["type"] != "string"
                || checked["result"]["value"] != "true"
            {
                return Err(actionable());
            }
        }
        let mut child = &frame.session;
        let (mut x, mut y) = (point.x, point.y);
        for (parent, child_frame) in &frame.ancestors {
            let viewport = engine.layout(child, deadline).await?;
            if viewport.width <= 0.
                || viewport.height <= 0.
                || x < 0.
                || y < 0.
                || x >= viewport.width
                || y >= viewport.height
            {
                return Err(actionable());
            }
            let owner = engine.frame_owner(parent, child_frame, deadline).await?;
            let q = engine.quad(&owner, deadline).await?;
            let (u, v) = (x / viewport.width, y / viewport.height);
            x = q[0] * (1. - u) * (1. - v)
                + q[2] * u * (1. - v)
                + q[4] * u * v
                + q[6] * (1. - u) * v;
            y = q[1] * (1. - u) * (1. - v)
                + q[3] * u * (1. - v)
                + q[5] * u * v
                + q[7] * (1. - u) * v;
            let hit = engine.hit_at(parent, Point { x, y }, deadline).await?;
            if hit.frame != *child_frame && hit.node != owner {
                return Err(actionable());
            }
            child = parent;
        }
        Ok(point)
    }
    async fn parents(
        &self,
        frames: &[Frame],
        deadline: Instant,
    ) -> Result<HashMap<String, Option<String>>> {
        let mut sessions = std::collections::HashSet::new();
        let mut parents = HashMap::new();
        for frame in frames {
            if !sessions.insert(frame.session.clone()) {
                continue;
            }
            let tree = self
                .adapter
                .wire
                .call(
                    Some(self.adapter.session(&frame.session)?),
                    Command::PageGetFrameTree,
                    json!({}),
                    deadline,
                )
                .await?;
            let mut todo = vec![(&tree["frameTree"], None::<String>)];
            while let Some((node, inherited)) = todo.pop() {
                let id = node["frame"]["id"].as_str().ok_or_else(stale)?.to_owned();
                let parent = node["frame"]["parentId"]
                    .as_str()
                    .map(str::to_owned)
                    .or(inherited);
                if parent.is_some() || !parents.contains_key(&id) {
                    parents.insert(id.clone(), parent);
                }
                if let Some(children) = node["childFrames"].as_array() {
                    for child in children {
                        todo.push((child, Some(id.clone())));
                    }
                }
                if parents.len() > sem::types::MAX_FRAMES {
                    return Err(Failure::new("browser_frame_limit"));
                }
            }
        }
        Ok(parents)
    }
    async fn world(&self, frame: &Frame, deadline: Instant) -> Result<i64> {
        let world_name = "rdh-semantic-dom-v1".to_owned();
        #[cfg(test)]
        let world_name = format!(
            "{world_name}-{}",
            self.realm_variant
                .load(std::sync::atomic::Ordering::Acquire)
        );
        let result=self.adapter.wire.call(Some(self.adapter.session(&frame.session)?),Command::PageCreateIsolatedWorld,
            json!({"frameId":self.adapter.key(&frame.id.0)?,"worldName":world_name,"grantUniveralAccess":false}),deadline).await?;
        result["executionContextId"]
            .as_i64()
            .ok_or_else(sem::types::decode)
    }
    async fn run(
        &self,
        frame: &Frame,
        world: i64,
        program: &Program,
        deadline: Instant,
    ) -> Result<Output> {
        let (operation, args) = program.encode();
        let reply=self.adapter.wire.call(Some(self.adapter.session(&frame.session)?),Command::RuntimeCallFunction(program.effect()),
            json!({"executionContextId":world,"functionDeclaration":sem::program::SOURCE,"arguments":[{"value":operation},{"value":args}],"returnByValue":true}),deadline).await?;
        decode_program(program, reply)
    }
    async fn stamp(
        &self,
        native: &Frame,
        parent: Option<sem::Context>,
        deadline: Instant,
    ) -> Result<sem::Frame> {
        let world = self.world(native, deadline).await?;
        let Output::Marker(marker) = self.run(native, world, &Program::Stamp, deadline).await?
        else {
            return Err(sem::types::decode());
        };
        let key = self.adapter.key(&native.id.0)?;
        self.identity.lock().unwrap().observe(
            key,
            parent,
            self.adapter.key(&native.document.0)?,
            &world.to_string(),
            marker,
        )
    }
    async fn key_pair(
        &self,
        session: &str,
        key: &str,
        code: &str,
        virtual_key: u32,
        modifiers: u8,
        deadline: Instant,
    ) -> Result<()> {
        self.leased_pair(session,InputPair::Key,
            json!({"type":"keyDown","key":key,"code":code,"windowsVirtualKeyCode":virtual_key,"modifiers":modifiers,"text":if key=="Enter"{"\r"}else{""}}),
            json!({"type":"keyUp","key":key,"code":code,"windowsVirtualKeyCode":virtual_key,"modifiers":modifiers}),deadline).await
    }
    #[cfg(test)]
    pub async fn fixture(&self, expression: &str, deadline: Instant) -> Result<Value> {
        let reply = self
            .adapter
            .wire
            .call(
                Some(self.adapter.session(&self.root)?),
                Command::RuntimeEvaluate(Effect::Page),
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
                deadline,
            )
            .await?;
        script_value(reply)
    }
}
impl Port for Semantic {
    fn coordination(&self) -> Arc<sem::Coordination> {
        self.coordination.clone()
    }
    fn authorize(&self, control: Arc<Control>, epoch: u64, human: bool) -> Self {
        Self {
            adapter: self.adapter.authorized(control, epoch, human),
            ..self.clone()
        }
    }
    fn tracked(&self) -> (Self, Arc<AtomicBool>) {
        let (adapter, flag) = self.adapter.track_effects();
        (
            Self {
                adapter,
                ..self.clone()
            },
            flag,
        )
    }
    fn connected(&self) -> bool {
        self.adapter.connected()
    }
    fn stop(&self) {
        self.adapter.stop()
    }
    fn pending(&self) -> usize {
        self.adapter.pending()
    }
    async fn frames(&self, deadline: Instant) -> Result<Vec<sem::Frame>> {
        let native = self.engine().page_frames(&self.root, deadline).await?;
        if native.len() > sem::types::MAX_FRAMES {
            return Err(Failure::new("browser_frame_limit"));
        }
        let mut output: Vec<sem::Frame> = Vec::new();
        let keys = native
            .iter()
            .map(|frame| self.adapter.key(&frame.id.0).map(str::to_owned))
            .collect::<Result<Vec<_>>>()?;
        self.identity.lock().unwrap().retain(&keys);
        let parents = self.parents(&native, deadline).await?;
        for frame in &native {
            let key = self.adapter.key(&frame.id.0)?.to_owned();
            self.routes
                .lock()
                .unwrap()
                .insert(key.clone(), frame.clone());
            let parent = parents
                .get(&key)
                .ok_or_else(stale)?
                .as_ref()
                .map(|key| self.identity.lock().unwrap().context(key))
                .transpose()?;
            output.push(self.stamp(frame, parent, deadline).await?);
        }
        self.identity.lock().unwrap().retain(&keys);
        self.routes
            .lock()
            .unwrap()
            .retain(|key, _| keys.contains(key));
        Ok(output)
    }
    async fn refresh(&self, frame: &sem::Frame, deadline: Instant) -> Result<sem::Frame> {
        let expected = self.native(frame)?;
        let frames = self.engine().page_frames(&self.root, deadline).await?;
        let native = frames
            .into_iter()
            .find(|f| f.id == expected.id)
            .ok_or_else(stale)?;
        self.routes
            .lock()
            .unwrap()
            .insert(self.adapter.key(&native.id.0)?.into(), native.clone());
        self.stamp(&native, frame.parent, deadline).await
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
        let native = self.native(frame)?;
        let world = self.world(&native, deadline).await?;
        let Output::Geometry(Some(mut geometry)) =
            self.run(&native, world, &program, deadline).await?
        else {
            return Err(sem::types::stale());
        };
        let mut child = native;
        let mut parent = frame.parent;
        let mut depth = 0;
        while let Some(context) = parent {
            depth += 1;
            if depth > sem::MAX_FRAMES {
                return Err(sem::types::decode());
            }
            let (key, next) = {
                let ids = self.identity.lock().unwrap();
                (ids.key(context)?.to_owned(), ids.parent(context)?)
            };
            let native = self
                .routes
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .ok_or_else(stale)?;
            let owner = self
                .engine()
                .frame_owner(&native.session, &child.id, deadline)
                .await?;
            let world = self.world(&native, deadline).await?;
            let session = Some(self.adapter.session(&native.session)?);
            let reply=self.adapter.wire.call(session,Command::DomResolveNode,json!({"backendNodeId":self.adapter.node(&owner)?,"executionContextId":world,"objectGroup":"rdh-semantic-geometry"}),deadline).await?;
            let object = reply["object"]["objectId"].as_str().ok_or_else(stale)?;
            let mapped=self.adapter.wire.call(session,Command::RuntimeCallFunction(Effect::None),json!({"executionContextId":world,"functionDeclaration":sem::program::SOURCE,"arguments":[{"value":"native_frame"},{"value":geometry},{"objectId":object}],"returnByValue":true}),deadline).await;
            let released = self
                .adapter
                .wire
                .cleanup()
                .call(
                    session,
                    Command::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    deadline.max(Instant::now() + std::time::Duration::from_secs(1)),
                )
                .await;
            if released.is_err() {
                self.adapter.stop();
                return Err(Failure::unknown("browser_cleanup_pending"));
            }
            let Output::Geometry(Some(mapped)) = decode_program(&program, mapped?)? else {
                return Err(Failure::new("browser_geometry_unsupported"));
            };
            geometry = mapped;
            child = native;
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
        let native = self.native(frame)?;
        let world = self.world(&native, deadline).await?;
        if let Program::Scroll { marker, node } = &program {
            let node = self.native_node(&native, marker, *node, deadline).await?;
            self.engine().scroll_into_view(&node, deadline).await?;
            return Ok(Output::Applied(Some(true)));
        }
        self.run(&native, world, &program, deadline).await
    }
    async fn input(
        &self,
        frame: &sem::Frame,
        input: sem::NativeInput,
        deadline: Instant,
    ) -> Result<()> {
        let native = self.native(frame)?;
        let session = self.adapter.session(&native.session)?;
        match input {
            sem::NativeInput::Click { node, .. } => {
                let token = node;
                let node = self
                    .native_node(&native, &frame.marker, node, deadline)
                    .await?;
                let Point { x, y } = self
                    .point(&native, &node, &frame.marker, token, deadline)
                    .await?;
                #[cfg(test)]
                self.coordination.before_click_dispatch().await;
                self.leased_pair(
                    session,
                    InputPair::Mouse,
                    json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
                    json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
                    deadline,
                )
                .await
            }
            sem::NativeInput::ReplaceText(text) => {
                self.key_pair(
                    session,
                    "a",
                    "KeyA",
                    65,
                    if cfg!(target_os = "macos") { 4 } else { 2 },
                    deadline,
                )
                .await?;
                self.key_pair(session, "Backspace", "Backspace", 8, 0, deadline)
                    .await?;
                self.adapter
                    .wire
                    .call(
                        Some(session),
                        Command::InputInsertText,
                        json!({"text":text}),
                        deadline,
                    )
                    .await
                    .map(drop)
            }
            sem::NativeInput::Press(key) => {
                let (key, code) = match key {
                    sem::Key::Enter => ("Enter", 13),
                    sem::Key::Tab => ("Tab", 9),
                    sem::Key::Escape => ("Escape", 27),
                    sem::Key::Backspace => ("Backspace", 8),
                    sem::Key::ArrowDown => ("ArrowDown", 40),
                    sem::Key::ArrowUp => ("ArrowUp", 38),
                    sem::Key::ArrowLeft => ("ArrowLeft", 37),
                    sem::Key::ArrowRight => ("ArrowRight", 39),
                };
                self.key_pair(session, key, key, code, 0, deadline).await
            }
        }
    }
}
fn decode_program(program: &Program, reply: Value) -> Result<Output> {
    if reply.get("exceptionDetails").is_some() {
        return Err(Failure::new("browser_script_exception"));
    }
    if reply["result"]["type"] != "string" {
        return Err(sem::types::decode());
    }
    program.decode(
        reply["result"]["value"]
            .as_str()
            .ok_or_else(sem::types::decode)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exceptions_missing_null_and_nonfinite_values_are_not_empty_observations() {
        for reply in [
            json!({"exceptionDetails":{},"result":{"type":"string","value":"null"}}),
            json!({"result":{"type":"undefined"}}),
            json!({"result":{"type":"object","subtype":"null","value":null}}),
            json!({"result":{"type":"number","unserializableValue":"NaN"}}),
            json!({"result":{"type":"string","value":"{}"}}),
        ] {
            assert!(decode_program(&Program::Stamp, reply).is_err());
        }
    }
}
