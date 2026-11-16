//! Native BiDi encoding for the shared semantic_dom_v1 coordinator.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))] // Native factory is Linux-only.
#[cfg(target_os = "linux")]
use super::Event;
use super::{
    Command, ContextId,
    command::Action,
    wire::{Bidi, Error, ProtocolError},
};
#[cfg(target_os = "linux")]
use crate::browser::semantic::policy::{Capabilities, Decisions, Policy};
use crate::browser::{
    Failure, Result,
    control::Control,
    semantic::{
        self as sem, Port,
        program::{Output, Program},
        types::Identities,
    },
};
use serde_json::{Value, json};
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
}

#[cfg(target_os = "linux")]
pub(in crate::browser) struct Session {
    pub port: Semantic,
    owner: super::session::Managed,
    policy: Policy,
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
        // This rejection precedes even native process/profile allocation.
        policy.admit(Capabilities {
            navigation_intercept: true,
            resolved_destination_check: true,
            observed_connection_address: false,
        })?;
        let mut owner = super::session::Managed::open(helper, browser, root)
            .await
            .map_err(failure)?;
        let channel = owner.channel.clone();
        let mut events = owner.events.take().unwrap();
        let boot = owner.boot.take().unwrap();
        let mut contexts = boot.contexts;
        let Some(root) = contexts.ids().first().cloned() else {
            owner.close(false).await.map_err(failure)?;
            return Err(sem::types::decode());
        };
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
                        Ok(event@Event::Created{..})=>{ledger.context_created();if !contexts.apply(&event){monitor.stop();break;}},
                        Ok(Event::Prompt{..})=>{monitor.stop();break;},
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
            coordination: Arc::new(sem::Coordination::new()),
        };
        Ok(Self {
            port,
            owner,
            policy,
            task,
            decisions,
        })
    }
    pub async fn navigate(&self, url: &str, deadline: Instant) -> Result<()> {
        if !self.policy.allows(url, deadline).await {
            return Err(Failure::new("browser_site_denied"));
        }
        self.port
            .channel
            .call(
                Command::Navigate {
                    context: self.port.root.clone(),
                    url: url.into(),
                },
                deadline,
            )
            .await
            .map(drop)
            .map_err(failure)
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
            match input {
                sem::NativeInput::Click { node, .. } => {
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
                    #[cfg(test)]
                    self.coordination.before_click_dispatch().await;
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
fn failure(error: Error) -> Failure {
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
