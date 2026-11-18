use super::{
    program::{Output, Program},
    *,
};
use crate::browser::{Failure, control::Control};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct Reference {
    scope: u64,
    generation: u64,
    index: usize,
    epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Continuation {
    scope: u64,
    generation: u64,
    epoch: u64,
}
#[derive(Serialize)]
pub(crate) struct Row {
    pub context: Context,
    pub facts: Facts,
    pub reference: Option<Reference>,
}
#[derive(Serialize)]
pub(crate) struct Observation {
    pub profile: &'static str,
    pub rows: Vec<Row>,
    pub cursor: Option<Continuation>,
    pub complete: bool,
    pub changed_during_read: bool,
    pub omitted: Vec<String>,
    pub scanned: usize,
}
#[derive(Clone)]
struct Element {
    frame: Frame,
    ancestors: Vec<Frame>,
    facts: Facts,
    fingerprint: [u8; 32],
}
#[derive(Clone)]
struct Cursor {
    frames: Vec<Frame>,
    ancestry: Vec<Frame>,
    top: Frame,
    query: Query,
    frame: usize,
    sequence: u64,
    opened: bool,
    last: Instant,
    omitted: Vec<String>,
    changed: bool,
}
#[derive(Default)]
struct State {
    generation: u64,
    refs: HashMap<Reference, Element>,
    cursor: Option<Cursor>,
    retained: Vec<Frame>,
    expires: Option<Instant>,
}
struct Inner<P> {
    port: P,
    coordination: Arc<Coordination>,
}
pub(crate) struct Coordination {
    scope: u64,
    control: Arc<Control>,
    state: Mutex<State>,
    operation: AsyncMutex<()>,
    input: AsyncMutex<()>,
    inputs: AtomicUsize,
    #[cfg(test)]
    click_dispatch: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}
impl Coordination {
    pub fn control(&self) -> Arc<Control> {
        self.control.clone()
    }
    pub fn pause(&self) {
        self.control.change(true);
        let mut state = self.state.lock().unwrap();
        state.refs.clear();
        state.cursor = None;
    }
    pub fn close(&self) {
        self.control.change(true);
        let mut state = self.state.lock().unwrap();
        state.refs.clear();
        state.cursor = None;
        state.retained.clear();
        state.expires = None;
    }
    pub fn new() -> Self {
        static SCOPES: AtomicU64 = AtomicU64::new(1);
        Self {
            scope: SCOPES.fetch_add(1, Ordering::Relaxed),
            control: Arc::new(Control::new()),
            state: Mutex::new(State::default()),
            operation: AsyncMutex::new(()),
            input: AsyncMutex::new(()),
            inputs: AtomicUsize::new(0),
            #[cfg(test)]
            click_dispatch: Mutex::new(None),
        }
    }
    #[cfg(test)]
    pub fn hold_click_dispatch(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let mut gate = self.click_dispatch.lock().unwrap();
        assert!(gate.is_none());
        *gate = Some((reached, wait));
        (ready, release)
    }
    #[cfg(test)]
    pub async fn before_click_dispatch(&self) {
        let gate = self.click_dispatch.lock().unwrap().take();
        if let Some((reached, wait)) = gate {
            let _ = reached.send(());
            let _ = wait.await;
        }
    }
}
impl<P> std::ops::Deref for Inner<P> {
    type Target = Coordination;
    fn deref(&self) -> &Coordination {
        &self.coordination
    }
}
pub(crate) struct Page<P>(Arc<Inner<P>>);
/// An internal, bounded grant to the current human controller. It contains no
/// native protocol handle and cannot authorize a different session or epoch.
pub(crate) struct HumanPermit {
    scope: u64,
    epoch: u64,
    expires: Instant,
}
#[cfg(test)]
impl HumanPermit {
    pub fn expire_for_test(&mut self) {
        self.expires = Instant::now();
    }
}
impl<P> Clone for Page<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Action {
    Click,
    Check(bool),
    ReplaceText(String),
    Press(Key),
    SetFormValue(String),
    AttachFile(crate::browser::files::Attachment),
}
impl Action {
    fn kind(&self) -> actionability::ActionKind {
        use actionability::ActionKind as Kind;
        match self {
            Self::Click => Kind::Click,
            Self::Check(_) => Kind::Check,
            Self::ReplaceText(_) => Kind::Fill,
            Self::Press(_) => Kind::Press,
            Self::SetFormValue(_) => Kind::Select,
            Self::AttachFile(_) => Kind::AttachFile,
        }
    }
}
#[derive(Debug, Serialize)]
pub(crate) struct Acknowledgement {
    pub changed: bool,
    pub input_acknowledged: bool,
    pub business_result_verified: bool,
    pub path: &'static str,
}

impl<P: Port> Page<P> {
    pub fn new(port: P) -> Self {
        let coordination = port.coordination();
        Self(Arc::new(Inner { port, coordination }))
    }
    pub fn control(&self) -> Arc<Control> {
        self.0.control.clone()
    }
    pub fn resources(&self) -> (usize, usize, usize) {
        let state = self.0.state.lock().unwrap();
        (
            state.refs.len(),
            self.0.inputs.load(Ordering::Acquire),
            self.0.port.pending(),
        )
    }
    /// Fence is synchronous and independent of the ordinary operation mutex.
    pub fn pause(&self) -> u64 {
        let epoch = self.0.control.change(true);
        let mut state = self.0.state.lock().unwrap();
        state.refs.clear();
        state.cursor = None;
        epoch
    }
    pub async fn settled(&self, deadline: Instant) -> Result<()> {
        while self.0.inputs.load(Ordering::Acquire) != 0 || self.0.port.pending() != 0 {
            if Instant::now() >= deadline {
                return Err(Failure::unknown("browser_cleanup_pending"));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if !self.0.port.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        Ok(())
    }
    pub async fn resume(&self, expected: u64, disclosure: bool, deadline: Instant) -> Result<u64> {
        if !disclosure {
            return Err(Failure::new("browser_disclosure_required"));
        }
        self.settled(deadline).await?;
        self.0.control.resume(expected)
    }
    pub async fn claim_human(&self, deadline: Instant) -> Result<HumanPermit> {
        let epoch = self.pause();
        self.settled(deadline).await?;
        if self.0.control.snapshot() != (epoch, true) {
            return Err(Failure::new("browser_control_changed"));
        }
        Ok(HumanPermit {
            scope: self.0.scope,
            epoch,
            expires: Instant::now() + Duration::from_secs(300),
        })
    }
    fn human(&self, permit: &HumanPermit) -> Result<u64> {
        if permit.scope != self.0.scope
            || Instant::now() >= permit.expires
            || self.0.control.snapshot() != (permit.epoch, true)
        {
            return Err(Failure::new("browser_control_changed"));
        }
        Ok(permit.epoch)
    }
    pub async fn return_to_ai(
        &self,
        permit: &HumanPermit,
        disclosure: bool,
        deadline: Instant,
    ) -> Result<u64> {
        let epoch = self.human(permit)?;
        self.resume(epoch, disclosure, deadline).await
    }
    fn authorization(&self, human: bool) -> Result<(P, u64)> {
        let (epoch, paused) = self.0.control.snapshot();
        if paused != human {
            return Err(Failure::new("browser_control_changed"));
        }
        Ok((
            self.0.port.authorize(self.0.control.clone(), epoch, human),
            epoch,
        ))
    }
    fn delivery(&self, epoch: u64, human: bool) -> Result<()> {
        if self.0.control.snapshot() != (epoch, human) {
            return Err(Failure::new("browser_control_changed"));
        }
        Ok(())
    }
    async fn expire(&self, port: &P, deadline: Instant) -> bool {
        let retained = {
            let mut state = self.0.state.lock().unwrap();
            if state.expires.is_none_or(|expires| expires > Instant::now()) {
                return false;
            }
            state.refs.clear();
            state.cursor = None;
            state.expires = None;
            std::mem::take(&mut state.retained)
        };
        for frame in retained {
            let _ = port.program(&frame, Program::Release, deadline).await;
        }
        true
    }
    #[cfg(test)]
    pub fn expire_for_test(&self) {
        self.0.state.lock().unwrap().expires = Some(Instant::now());
    }
    pub async fn observe(
        &self,
        query: Query,
        nodes: usize,
        characters: usize,
        cursor: Option<Continuation>,
        deadline: Instant,
    ) -> Result<Observation> {
        self.observe_as(query, nodes, characters, cursor, false, deadline)
            .await
    }
    pub async fn human_observe(
        &self,
        permit: &HumanPermit,
        query: Query,
        nodes: usize,
        characters: usize,
        deadline: Instant,
    ) -> Result<Observation> {
        self.human(permit)?;
        let observed = self
            .observe_as(query, nodes, characters, None, true, deadline)
            .await?;
        self.human(permit)?;
        Ok(observed)
    }
    async fn observe_as(
        &self,
        query: Query,
        nodes: usize,
        characters: usize,
        cursor: Option<Continuation>,
        human: bool,
        deadline: Instant,
    ) -> Result<Observation> {
        query.validate()?;
        if !(1..=MAX_NODES).contains(&nodes) || !(1024..=MAX_CHARACTERS).contains(&characters) {
            return Err(Failure::new("browser_invalid_params"));
        }
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let _input = tokio::time::timeout_at(deadline.into(), self.0.input.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(human)?;
        if self.expire(&port, deadline).await && cursor.is_some() {
            return Err(types::stale());
        }
        let retained = {
            let mut state = self.0.state.lock().unwrap();
            if let Some(token) = &cursor
                && (token.scope != self.0.scope
                    || token.epoch != epoch
                    || token.generation != state.generation
                    || state.cursor.as_ref().is_none_or(|scan| {
                        scan.query != query || scan.last.elapsed() > Duration::from_secs(300)
                    }))
            {
                return Err(types::stale());
            }
            state.refs.clear();
            std::mem::take(&mut state.retained)
        };
        for frame in retained {
            let _ = port
                .program(
                    &frame,
                    Program::ClearNodes {
                        marker: frame.marker.clone(),
                    },
                    deadline,
                )
                .await;
        }
        let mut scan = if let Some(token) = cursor {
            let state = self.0.state.lock().unwrap();
            if token.scope != self.0.scope
                || token.epoch != epoch
                || token.generation != state.generation
            {
                return Err(types::stale());
            }
            let scan = state.cursor.clone().ok_or_else(types::stale)?;
            if scan.query != query || scan.last.elapsed() > Duration::from_secs(300) {
                return Err(types::stale());
            }
            scan
        } else {
            let old = self.0.state.lock().unwrap().cursor.take();
            if let Some(old) = old {
                for frame in &old.frames {
                    let _ = port.program(frame, Program::Release, deadline).await;
                }
            }
            let frames = port.frames(deadline).await?;
            if frames.is_empty() || frames.len() > MAX_FRAMES {
                return Err(Failure::new("browser_frame_limit"));
            }
            let top = frames[0].clone();
            let ancestry = frames.clone();
            let frames = frames
                .into_iter()
                .filter(|f| query.within.is_none_or(|id| id == f.context))
                .collect::<Vec<_>>();
            if frames.is_empty() {
                return Err(types::stale());
            }
            Cursor {
                frames,
                ancestry,
                top,
                query: query.clone(),
                frame: 0,
                sequence: 0,
                opened: false,
                last: Instant::now(),
                omitted: Vec::new(),
                changed: false,
            }
        };
        same_document(&scan.top, &port.refresh(&scan.top, deadline).await?)?;
        let generation = {
            let mut state = self.0.state.lock().unwrap();
            state.refs.clear();
            state.cursor = None;
            state.generation = state.generation.checked_add(1).ok_or_else(types::decode)?;
            state.generation
        };
        let mut observation = Observation {
            profile: "semantic_dom_v1",
            rows: Vec::new(),
            cursor: None,
            complete: false,
            changed_during_read: scan.changed,
            omitted: scan.omitted.clone(),
            scanned: 0,
        };
        let mut remaining = characters;
        let mut refs = HashMap::new();
        while scan.frame < scan.frames.len() && observation.rows.len() < nodes && remaining >= 1024
        {
            let frame = &scan.frames[scan.frame];
            let current = port.refresh(frame, deadline).await?;
            same_document(frame, &current)?;
            if !scan.opened {
                let Output::Marker(marker) = port
                    .program(frame, Program::Open(query.clone()), deadline)
                    .await?
                else {
                    return Err(types::decode());
                };
                if marker.realm != frame.marker.realm
                    || marker.restoration != frame.marker.restoration
                {
                    return Err(types::stale());
                }
                scan.opened = true;
            }
            let Output::Batch(batch) = port
                .program(
                    frame,
                    Program::Next {
                        marker: frame.marker.clone(),
                        sequence: scan.sequence,
                        nodes: nodes - observation.rows.len(),
                        characters: remaining,
                    },
                    deadline,
                )
                .await?
            else {
                return Err(types::decode());
            };
            if batch.stamp.realm != frame.marker.realm
                || batch.stamp.restoration != frame.marker.restoration
                || batch.sequence != scan.sequence + 1
            {
                return Err(types::stale());
            }
            scan.sequence = batch.sequence;
            {
                let mut state = self.0.state.lock().unwrap();
                if !state
                    .retained
                    .iter()
                    .any(|known| known.context == frame.context)
                {
                    state.retained.push(frame.clone());
                }
            }
            remaining = remaining.saturating_sub(batch.characters);
            observation.scanned += batch.scanned;
            observation.changed_during_read |= batch.changed;
            observation.omitted.extend(batch.omitted);
            let candidates = batch
                .rows
                .iter()
                .filter(|facts| facts.eligible())
                .map(|facts| facts.node)
                .collect::<Vec<_>>();
            let mut checked = HashMap::new();
            if !candidates.is_empty() {
                let Output::Checks(values) = port
                    .program(
                        frame,
                        Program::Recheck {
                            marker: frame.marker.clone(),
                            nodes: candidates.clone(),
                        },
                        deadline,
                    )
                    .await?
                else {
                    return Err(types::decode());
                };
                for (node, value) in candidates.into_iter().zip(values) {
                    checked.insert(node, value);
                }
            }
            let stable_document =
                same_document(frame, &port.refresh(frame, deadline).await?).is_ok();
            if !stable_document {
                observation.changed_during_read = true;
                observation
                    .omitted
                    .push("document_changed_during_read".into());
            }
            for facts in batch.rows {
                let stable = checked
                    .get(&facts.node)
                    .and_then(Option::as_ref)
                    .is_some_and(|current| current.fingerprint() == facts.fingerprint());
                if facts.eligible() && !stable {
                    observation.changed_during_read = true;
                }
                let reference =
                    (facts.eligible() && stable && stable_document).then(|| Reference {
                        scope: self.0.scope,
                        generation,
                        index: observation.rows.len(),
                        epoch,
                    });
                if let Some(reference) = &reference {
                    // Parent documents are rechecked before an action. A child
                    // navigation need not invalidate siblings' references.
                    let mut ancestors = Vec::new();
                    let mut parent = frame.parent;
                    while let Some(context) = parent {
                        let ancestor = scan
                            .ancestry
                            .iter()
                            .find(|f| f.context == context)
                            .or_else(|| (scan.top.context == context).then_some(&scan.top))
                            .ok_or_else(types::stale)?;
                        ancestors.push(ancestor.clone());
                        parent = ancestor.parent;
                        if ancestors.len() > MAX_FRAMES {
                            return Err(types::decode());
                        }
                    }
                    refs.insert(
                        reference.clone(),
                        Element {
                            frame: frame.clone(),
                            ancestors,
                            fingerprint: facts.fingerprint(),
                            facts: facts.clone(),
                        },
                    );
                }
                observation.rows.push(Row {
                    context: frame.context,
                    facts,
                    reference,
                });
            }
            if !batch.more {
                scan.frame += 1;
                scan.sequence = 0;
                scan.opened = false;
            } else {
                break;
            }
        }
        same_document(&scan.top, &port.refresh(&scan.top, deadline).await?)?;
        let after = port.frames(deadline).await?;
        if after.len() != scan.ancestry.len()
            || scan.ancestry.iter().any(|before| {
                !after.iter().any(|current| {
                    current.context == before.context
                        && current.parent == before.parent
                        && current.document == before.document
                        && current.realm == before.realm
                })
            })
        {
            observation.changed_during_read = true;
            observation
                .omitted
                .push("frame_tree_changed_during_read".into());
            refs.retain(|_, element| {
                after.iter().any(|current| {
                    current.context == element.frame.context
                        && current.document == element.frame.document
                        && current.realm == element.frame.realm
                })
            });
        }
        observation.complete = scan.frame == scan.frames.len()
            && !observation.omitted.iter().any(|s| {
                matches!(
                    s.as_str(),
                    "row_character_limit"
                        | "traversal_limit"
                        | "text_character_limit"
                        | "document_changed_during_read"
                        | "frame_tree_changed_during_read"
                )
            });
        observation.omitted.sort();
        observation.omitted.dedup();
        scan.last = Instant::now();
        scan.omitted = observation.omitted.clone();
        scan.changed = observation.changed_during_read;
        self.delivery(epoch, human)?;
        let mut state = self.0.state.lock().unwrap();
        // Pause may race the delivery check: serialize publication with the
        // same control mutex used by transport submissions.
        let control = self.0.control.state.lock().unwrap();
        if *control != (epoch, human) {
            return Err(Failure::new("browser_control_changed"));
        }
        state.refs = refs;
        state.expires = Some(Instant::now() + Duration::from_secs(300));
        if scan.frame < scan.frames.len() {
            observation.cursor = Some(Continuation {
                scope: self.0.scope,
                generation,
                epoch,
            });
            state.cursor = Some(scan);
        }
        Ok(observation)
    }
    pub async fn act(
        &self,
        reference: Reference,
        action: Action,
        deadline: Instant,
    ) -> Result<Acknowledgement> {
        self.act_as(reference, action, false, deadline).await
    }
    pub async fn evidence(
        &self,
        query: &super::evidence::Query,
        target: Option<super::evidence::Target>,
        deadline: Instant,
    ) -> Result<super::evidence::Capture> {
        let deadline = deadline.min(Instant::now() + super::evidence::TIMEOUT);
        if Instant::now() >= deadline {
            return Err(Failure::new("browser_deadline"));
        }
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        let capture = tokio::time::timeout_at(
            deadline.into(),
            super::evidence::capture(&port, query, target, deadline),
        )
        .await
        .map_err(|_| Failure::unknown("browser_timeout_unknown"))??;
        self.delivery(epoch, false)?;
        Ok(capture)
    }
    pub async fn diagnose(
        &self,
        reference: Reference,
        kind: actionability::ActionKind,
        deadline: Instant,
    ) -> Result<actionability::Diagnostic> {
        let deadline = deadline.min(Instant::now() + actionability::TIMEOUT);
        if Instant::now() >= deadline {
            return Err(Failure::new("browser_deadline"));
        }
        // This ordinary lock does not participate in the synchronous control
        // fence. No input permit or upload authorization is acquired here.
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        if reference.scope != self.0.scope || reference.epoch != epoch {
            return Err(types::stale());
        }
        let element = {
            let state = self.0.state.lock().unwrap();
            if state
                .expires
                .is_some_and(|expires| expires <= Instant::now())
            {
                return Err(types::stale());
            }
            state
                .refs
                .get(&reference)
                .cloned()
                .ok_or_else(types::stale)?
        };
        let task = async {
            let facts = inspect(&port, &element, deadline).await?;
            let top_level = element.frame.parent.is_none() && element.ancestors.is_empty();
            if !actionability::requirements(&facts, kind, top_level).is_empty() {
                return Ok(actionability::assess(&facts, kind, top_level, None, None));
            }
            let geometry = match port.target(&element.frame, facts.node, deadline).await {
                Ok(geometry) => geometry,
                Err(error) if error.code == "browser_geometry_unsupported" => {
                    return Ok(actionability::assess(&facts, kind, top_level, None, None));
                }
                Err(error) => return Err(error),
            };
            if !geometry.visible
                || !geometry.in_view
                || !facts.in_view
                || !geometry.hit
                || !facts.hit
            {
                return Ok(actionability::assess(
                    &facts,
                    kind,
                    top_level,
                    Some(&geometry),
                    None,
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let current = inspect(&port, &element, deadline).await?;
            let current_geometry = match port.target(&element.frame, current.node, deadline).await {
                Ok(geometry) => geometry,
                Err(error) if error.code == "browser_geometry_unsupported" => {
                    return Ok(actionability::assess(&current, kind, top_level, None, None));
                }
                Err(error) => return Err(error),
            };
            Ok(actionability::assess(
                &facts,
                kind,
                top_level,
                Some(&geometry),
                Some((&current, &current_geometry)),
            ))
        };
        // A cancelled or timed-out read leaves submitted native commands and
        // their late replies under the existing transport ownership rules.
        let result = tokio::time::timeout_at(deadline.into(), task)
            .await
            .map_err(|_| Failure::unknown("browser_timeout_unknown"))??;
        self.delivery(epoch, false)?;
        Ok(result)
    }
    pub async fn human_act(
        &self,
        permit: &HumanPermit,
        reference: Reference,
        action: Action,
        deadline: Instant,
    ) -> Result<Acknowledgement> {
        self.human(permit)?;
        if reference.epoch != permit.epoch {
            return Err(types::stale());
        }
        self.act_as(reference, action, true, deadline).await
    }
    async fn act_as(
        &self,
        reference: Reference,
        action: Action,
        human: bool,
        deadline: Instant,
    ) -> Result<Acknowledgement> {
        let page = self.clone();
        self.0
            .inputs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 32).then_some(count + 1)
            })
            .map_err(|_| Failure::new("browser_busy"))?;
        struct Caller<P: Port> {
            page: Page<P>,
            armed: bool,
        }
        impl<P: Port> Drop for Caller<P> {
            fn drop(&mut self) {
                if self.armed {
                    self.page.pause();
                }
            }
        }
        let mut caller = Caller {
            page: self.clone(),
            armed: true,
        };
        // Dropping the caller leaves this owned task, its lease and its native
        // command receipts alive through cleanup and settlement.
        let task = tokio::spawn(async move {
            struct Owner<P>(Arc<Inner<P>>);
            impl<P> Drop for Owner<P> {
                fn drop(&mut self) {
                    self.0.inputs.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _owner = Owner(page.0.clone());
            page.execute(reference, action, human, deadline).await
        });
        let result = task.await.map_err(|_| Failure::unknown("browser_lost"))?;
        caller.armed = false;
        result
    }
    async fn execute(
        &self,
        reference: Reference,
        action: Action,
        human: bool,
        deadline: Instant,
    ) -> Result<Acknowledgement> {
        let _observation = if human {
            None
        } else {
            Some(
                tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
                    .await
                    .map_err(|_| Failure::new("browser_busy"))?,
            )
        };
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.input.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(human)?;
        if self.expire(&port, deadline).await {
            return Err(types::stale());
        }
        if reference.scope != self.0.scope || reference.epoch != epoch {
            return Err(types::stale());
        }
        let element = self
            .0
            .state
            .lock()
            .unwrap()
            .refs
            .get(&reference)
            .cloned()
            .ok_or_else(types::stale)?;
        let (port, effects) = port.tracked();
        let result = perform(&port, &element, action, deadline).await;
        match result {
            Ok(result) => {
                self.delivery(epoch, human).map_err(|mut error| {
                    if effects.load(Ordering::Acquire) {
                        error.state = "unknown";
                    }
                    error
                })?;
                Ok(result)
            }
            Err(mut error) => {
                if effects.load(Ordering::Acquire) || error.state == "unknown" {
                    error.state = "unknown";
                    self.pause();
                }
                Err(error)
            }
        }
    }
    /// Run a typed session/surface mutation under the same cancellation and
    /// effect fence as reference input. Dialog recovery alone bypasses the
    /// pending-input mutex, since that input may be waiting for this dialog.
    pub async fn command<T, F, Fut>(
        &self,
        human: bool,
        emergency: bool,
        deadline: Instant,
        operation: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(P) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        self.0
            .inputs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 32).then_some(n + 1)
            })
            .map_err(|_| Failure::new("browser_busy"))?;
        struct Caller<P: Port> {
            page: Page<P>,
            armed: bool,
        }
        impl<P: Port> Drop for Caller<P> {
            fn drop(&mut self) {
                if self.armed {
                    self.page.pause();
                }
            }
        }
        let mut caller = Caller {
            page: self.clone(),
            armed: true,
        };
        let page = self.clone();
        let task = tokio::spawn(async move {
            struct Owner<P>(Arc<Inner<P>>);
            impl<P> Drop for Owner<P> {
                fn drop(&mut self) {
                    self.0.inputs.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _owner = Owner(page.0.clone());
            let _serial = if emergency {
                None
            } else {
                Some(
                    tokio::time::timeout_at(deadline.into(), page.0.input.lock())
                        .await
                        .map_err(|_| Failure::new("browser_busy"))?,
                )
            };
            let (port, epoch) = page.authorization(human)?;
            {
                let mut state = page.0.state.lock().unwrap();
                state.refs.clear();
                state.cursor = None;
            }
            let (port, effects) = port.tracked();
            let result = operation(port).await;
            let result = result.and_then(|value| {
                page.delivery(epoch, human)?;
                Ok(value)
            });
            result.map_err(|mut error| {
                if effects.load(Ordering::Acquire) || error.state == "unknown" {
                    error.state = "unknown";
                    if !human {
                        page.pause();
                    }
                }
                error
            })
        });
        let result = task.await.map_err(|_| Failure::unknown("browser_lost"))?;
        caller.armed = false;
        result
    }
    pub fn access(&self, human: bool) -> Result<P> {
        self.authorization(human).map(|(port, _)| port)
    }
    pub fn check_human(&self, permit: &HumanPermit) -> Result<()> {
        self.human(permit).map(drop)
    }
    pub fn renew_human(&self, permit: &mut HumanPermit) -> Result<()> {
        self.human(permit)?;
        permit.expires = Instant::now() + Duration::from_secs(300);
        Ok(())
    }

    pub async fn release(&self, deadline: Instant) -> Result<()> {
        self.pause();
        self.settled(deadline).await?;
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::unknown("browser_cleanup_pending"))?;
        let frames = self.0.port.frames(deadline).await?;
        for frame in frames {
            self.0
                .port
                .program(&frame, Program::Release, deadline)
                .await?;
        }
        Ok(())
    }
}
fn same_document(expected: &Frame, actual: &Frame) -> Result<()> {
    if expected.context != actual.context
        || expected.document != actual.document
        || expected.realm != actual.realm
    {
        return Err(types::stale());
    }
    Ok(())
}
async fn inspect<P: Port>(port: &P, element: &Element, deadline: Instant) -> Result<Facts> {
    for ancestor in &element.ancestors {
        same_document(ancestor, &port.refresh(ancestor, deadline).await?)?;
    }
    same_document(
        &element.frame,
        &port.refresh(&element.frame, deadline).await?,
    )?;
    let Output::Facts(Some(facts)) = port
        .program(
            &element.frame,
            Program::Inspect {
                marker: element.frame.marker.clone(),
                node: element.facts.node,
            },
            deadline,
        )
        .await?
    else {
        return Err(types::stale());
    };
    if facts.fingerprint() != element.fingerprint || !facts.eligible() {
        return Err(types::stale());
    }
    Ok(*facts)
}
async fn perform<P: Port>(
    port: &P,
    element: &Element,
    action: Action,
    deadline: Instant,
) -> Result<Acknowledgement> {
    let mut facts = inspect(port, element, deadline).await?;
    let kind = action.kind();
    let top_level = element.frame.parent.is_none() && element.ancestors.is_empty();
    if facts.kind == "inferred" && !matches!(action, Action::Click | Action::Press(_)) {
        return Err(Failure::new("browser_not_actionable"));
    }
    match &action {
        Action::AttachFile(_) => {
            if !top_level {
                return Err(Failure::new("browser_upload_frame_unsupported"));
            }
            if !actionability::requirements(&facts, kind, top_level).is_empty() {
                return Err(Failure::new("browser_not_actionable"));
            }
        }
        Action::ReplaceText(value) => {
            if value.chars().count() > 4096 {
                return Err(Failure::new("browser_invalid_params"));
            }
            if !actionability::requirements(&facts, kind, top_level).is_empty() {
                return Err(Failure::new("browser_not_actionable"));
            }
            if value.contains('\n') && facts.tag == "input" {
                return Err(Failure::new("browser_invalid_params"));
            }
            if facts.value.as_ref() == Some(value) {
                return Ok(ack(false, "native_edit"));
            }
        }
        Action::Check(value) => {
            if !actionability::requirements(&facts, kind, top_level).is_empty() {
                return Err(Failure::new("browser_not_actionable"));
            }
            if facts.checked == *value {
                return Ok(ack(false, "native_pointer"));
            }
        }
        Action::SetFormValue(value) => {
            if value.len() > 4096 || facts.sensitive || facts.readonly {
                return Err(Failure::new("browser_invalid_params"));
            }
            if !actionability::native_form(&facts) {
                return Err(Failure::new("browser_not_actionable"));
            }
            if !matches!(
                port.program(
                    &element.frame,
                    Program::FormCheck {
                        marker: element.frame.marker.clone(),
                        node: facts.node,
                        value: value.clone()
                    },
                    deadline
                )
                .await?,
                Output::Applied(Some(true))
            ) {
                return Err(Failure::new("browser_invalid_params"));
            }
            if facts.value.as_ref() == Some(value) {
                return Ok(ack(false, "synthetic_form"));
            }
        }
        _ => {}
    }
    let marker = element.frame.marker.clone();
    let node = facts.node;
    let mut geometry = port.target(&element.frame, node, deadline).await?;
    if !geometry.visible || geometry.in_view && !geometry.hit {
        return Err(Failure::new("browser_not_actionable"));
    }
    if !facts.in_view || !geometry.in_view {
        if !matches!(
            port.program(
                &element.frame,
                Program::Scroll {
                    marker: marker.clone(),
                    node
                },
                deadline
            )
            .await?,
            Output::Applied(Some(true))
        ) {
            return Err(Failure::new("browser_not_actionable"));
        }
        facts = inspect(port, element, deadline).await?;
        geometry = port.target(&element.frame, node, deadline).await?;
    }
    if !geometry.hit || !geometry.in_view || !facts.hit || !facts.visible || !facts.enabled {
        return Err(Failure::new("browser_not_actionable"));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let steady = inspect(port, element, deadline).await?;
    if !steady.hit || steady.x != facts.x || steady.y != facts.y {
        return Err(Failure::new("browser_not_actionable"));
    }
    let final_geometry = port.target(&element.frame, node, deadline).await?;
    if actionability::assess(
        &facts,
        kind,
        top_level,
        Some(&geometry),
        Some((&steady, &final_geometry)),
    )
    .status
        != actionability::Status::ReadyNow
    {
        return Err(Failure::new("browser_not_actionable"));
    }
    facts = steady;
    let expected_check = match &action {
        Action::Check(value) => Some(*value),
        _ => None,
    };
    let path = match action {
        Action::AttachFile(attachment) => {
            let Output::Surface(meta) = port
                .program(&element.frame, Program::Surface, deadline)
                .await?
            else {
                return Err(types::decode());
            };
            let origin = crate::browser::page::origin(&meta.url)?;
            inspect(port, element, deadline).await?;
            let path = attachment.consume(origin)?;
            port.input(
                &element.frame,
                NativeInput::SelectFile {
                    node: facts.node,
                    path,
                },
                deadline,
            )
            .await?;
            "native_file"
        }
        Action::Click | Action::Check(_) => {
            port.input(
                &element.frame,
                NativeInput::Click {
                    node: facts.node,
                    x: facts.x,
                    y: facts.y,
                },
                deadline,
            )
            .await?;
            if let Some(expected) = expected_check {
                same_document(
                    &element.frame,
                    &port.refresh(&element.frame, deadline).await?,
                )?;
                let Output::Facts(Some(after)) = port
                    .program(
                        &element.frame,
                        Program::Inspect {
                            marker: element.frame.marker.clone(),
                            node,
                        },
                        deadline,
                    )
                    .await?
                else {
                    return Err(Failure::unknown("browser_check_not_confirmed"));
                };
                if after.checked != expected {
                    return Err(Failure::unknown("browser_check_not_confirmed"));
                }
            }
            "native_pointer"
        }
        Action::ReplaceText(value) => {
            if !matches!(
                port.program(
                    &element.frame,
                    Program::Focus {
                        marker: marker.clone(),
                        node
                    },
                    deadline
                )
                .await?,
                Output::Applied(Some(true))
            ) {
                return Err(Failure::new("browser_not_actionable"));
            }
            inspect(port, element, deadline).await?;
            port.input(
                &element.frame,
                NativeInput::ReplaceText(value.clone()),
                deadline,
            )
            .await?;
            same_document(
                &element.frame,
                &port.refresh(&element.frame, deadline).await?,
            )?;
            let Output::Facts(Some(after)) = port
                .program(
                    &element.frame,
                    Program::Inspect {
                        marker: element.frame.marker.clone(),
                        node,
                    },
                    deadline,
                )
                .await?
            else {
                return Err(Failure::unknown("browser_edit_not_confirmed"));
            };
            if after.value.as_ref() != Some(&value) {
                return Err(Failure::unknown("browser_edit_not_confirmed"));
            }
            "native_edit"
        }
        Action::Press(key) => {
            if !matches!(
                port.program(&element.frame, Program::Focus { marker, node }, deadline)
                    .await?,
                Output::Applied(Some(true))
            ) {
                return Err(Failure::new("browser_not_actionable"));
            }
            inspect(port, element, deadline).await?;
            port.input(&element.frame, NativeInput::Press(key), deadline)
                .await?;
            "native_key"
        }
        Action::SetFormValue(value) => {
            match port
                .program(
                    &element.frame,
                    Program::Form {
                        marker,
                        node,
                        value,
                    },
                    deadline,
                )
                .await?
            {
                Output::Applied(Some(changed)) => return Ok(ack(changed, "synthetic_form")),
                _ => return Err(Failure::new("browser_not_actionable")),
            }
        }
    };
    Ok(ack(true, path))
}
fn ack(changed: bool, path: &'static str) -> Acknowledgement {
    Acknowledgement {
        changed,
        input_acknowledged: true,
        business_result_verified: false,
        path,
    }
}
