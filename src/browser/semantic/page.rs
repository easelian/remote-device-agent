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
    reuse_radio: Option<u64>,
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
    pub fn pause(&self) -> u64 {
        let epoch = self.control.change(true);
        self.clear_ai_refs();
        epoch
    }
    /// Interrupt a busy AI operation before waiting for the session. An
    /// already paused epoch may belong to a human input or an in-flight claim.
    pub fn fence_active_ai(&self) -> u64 {
        let epoch = {
            let mut control = self.control.state.lock().unwrap();
            if control.1 {
                return control.0;
            }
            control.0 += 1;
            control.1 = true;
            control.0
        };
        self.clear_ai_refs();
        epoch
    }
    fn clear_ai_refs(&self) {
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
    Scroll { delta: f64 },
    Hover,
    Click,
    Check(bool),
    ReplaceText(String),
    Press(Key),
    SetFormValue(String),
    AttachFile(crate::browser::files::Attachment),
    Drag { target: Reference },
}
impl Action {
    fn kind(&self) -> actionability::ActionKind {
        use actionability::ActionKind as Kind;
        match self {
            Self::Scroll { .. } => Kind::Scroll,
            Self::Hover => Kind::Hover,
            Self::Click => Kind::Click,
            Self::Check(_) => Kind::Check,
            Self::ReplaceText(_) => Kind::Fill,
            Self::Press(_) => Kind::Press,
            Self::SetFormValue(_) => Kind::Select,
            Self::AttachFile(_) => Kind::AttachFile,
            Self::Drag { .. } => unreachable!("drag validates two endpoint grants"),
        }
    }
}
#[derive(Clone, Copy, PartialEq)]
enum ValidationProbe {
    Disabled,
    Reasons,
    Field,
}

pub(crate) struct SubmitFeedback {
    pub kind: super::program::SubmitFeedbackKind,
    pub field: Option<super::evidence::Target>,
    pub message: super::evidence::Target,
    pub reason: String,
}
pub(crate) struct SubmitField {
    pub field: super::evidence::Target,
    pub message: super::evidence::Target,
    pub reason: String,
    pub association: super::program::SubmitFieldAssociation,
}
pub(crate) struct SubmitVisualField {
    pub field: super::evidence::Target,
    pub changes: Vec<super::program::SubmitVisualChange>,
}
pub(crate) struct SubmitIndicator {
    pub field: super::evidence::Target,
    pub source: super::evidence::Target,
    pub kind: super::program::SubmitIndicatorKind,
}

pub(crate) struct SubmitRecordBinding {
    pub endpoint: super::task_events::RequestEndpoint,
    pub frame: Frame,
    pub submitter: u64,
    pub record: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Acknowledgement {
    pub changed: bool,
    pub input_acknowledged: bool,
    pub business_result_verified: bool,
    pub path: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_validation: Option<super::evidence::Validation>,
    #[serde(skip)]
    pub(crate) native_validation_target: Option<super::evidence::Target>,
}

impl<P: Port> Page<P> {
    pub fn new(port: P) -> Self {
        let coordination = port.coordination();
        Self(Arc::new(Inner { port, coordination }))
    }
    pub fn control(&self) -> Arc<Control> {
        self.0.control.clone()
    }
    #[cfg(test)]
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
    pub fn input_pending(&self) -> bool {
        self.0.inputs.load(Ordering::Acquire) != 0 || self.0.port.pending() != 0
    }
    pub async fn settled(&self, deadline: Instant) -> Result<()> {
        while self.input_pending() {
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
                .filter(|f| query.filter != Filter::DragEndpoints || f.parent.is_none())
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
                .filter(|facts| facts.eligible_for(&query.filter))
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
                            endpoint: query.filter == Filter::DragEndpoints,
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
                if facts.eligible_for(&query.filter) && !stable {
                    observation.changed_during_read = true;
                }
                let reference = (facts.eligible_for(&query.filter) && stable && stable_document)
                    .then(|| Reference {
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
                            reuse_radio: None,
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
        self.act_as(
            reference,
            action,
            false,
            deadline,
            ValidationProbe::Disabled,
            None,
        )
        .await
    }
    /// Return only a ref from the original observation. Losing an optional
    /// continuation after the radio input must not change its write receipt.
    pub async fn reusable_submit(&self, radio: &Reference, deadline: Instant) -> Option<Reference> {
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .ok()?;
        let (port, epoch) = self.authorization(false).ok()?;
        let (frame, node) = {
            let state = self.0.state.lock().unwrap();
            let element = state.refs.get(radio)?;
            let f = &element.facts;
            if radio.epoch != epoch
                || element.frame.parent.is_none()
                || f.kind != "native"
                || f.tag != "input"
                || f.input_type != "radio"
                || f.checked
            {
                return None;
            }
            (element.frame.clone(), f.node)
        };
        let proof = Program::ReusableSubmit {
            marker: frame.marker.clone(),
            radio: node,
        };
        let Output::ReusableSubmit(Some(submit)) =
            port.program(&frame, proof, deadline).await.ok()?
        else {
            return None;
        };
        let (reference, element) = {
            let state = self.0.state.lock().unwrap();
            let (r, e) = state.refs.iter().find(|(r, e)| {
                r.generation == radio.generation
                    && r.epoch == epoch
                    && e.frame == frame
                    && e.facts.node == submit
                    && e.facts.kind == "native"
                    && e.facts.role == "button"
            })?;
            (r.clone(), e.clone())
        };
        inspect(&port, &element, deadline).await.ok()?;
        self.delivery(epoch, false).ok()?;
        self.0
            .state
            .lock()
            .unwrap()
            .refs
            .get_mut(&reference)?
            .reuse_radio = Some(node);
        Some(reference)
    }
    pub async fn act_with_diagnostics(
        &self,
        reference: Reference,
        action: Action,
        read_validation: bool,
        register_field: bool,
        submit_window: Option<task_events::Sink>,
        deadline: Instant,
    ) -> Result<Acknowledgement> {
        let probe = if register_field {
            ValidationProbe::Field
        } else if read_validation {
            ValidationProbe::Reasons
        } else {
            ValidationProbe::Disabled
        };
        self.act_as(reference, action, false, deadline, probe, submit_window)
            .await
    }
    pub async fn prepare_submit_record(
        &self,
        reference: Reference,
        target: super::evidence::Target,
        anchor_text: &str,
        expected_text: Option<&str>,
        deadline: Instant,
    ) -> Result<SubmitRecordBinding> {
        if anchor_text.is_empty()
            || anchor_text.chars().count() > 128
            || expected_text.is_some_and(|text| text.is_empty() || text.chars().count() > 128)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        if reference.epoch != epoch {
            return Err(types::stale());
        }
        let element = {
            let state = self.0.state.lock().unwrap();
            state
                .refs
                .get(&reference)
                .cloned()
                .ok_or_else(types::stale)?
        };
        if element.frame.parent.is_some() || target.context != element.frame.context {
            return Err(types::stale());
        }
        let frames = port.frames(deadline).await?;
        let actual_target = super::evidence::target(&frames, &element.frame, target.node)?;
        if actual_target.context != target.context || actual_target.digest != target.digest {
            return Err(types::stale());
        }
        let Output::SubmitEndpoint(Some(url)) = port
            .program(
                &element.frame,
                Program::SubmitRecordArm {
                    marker: element.frame.marker.clone(),
                    submitter: element.facts.node,
                    record: target.node,
                    anchor_text: anchor_text.to_owned(),
                    expected_text: expected_text.map(str::to_owned),
                },
                deadline,
            )
            .await?
        else {
            return Err(Failure::new("browser_not_actionable"));
        };
        let endpoint = super::task_events::RequestEndpoint::from_url(&url)
            .ok_or_else(|| Failure::new("browser_not_actionable"))?;
        self.delivery(epoch, false)?;
        Ok(SubmitRecordBinding {
            endpoint,
            frame: element.frame,
            submitter: element.facts.node,
            record: target.node,
        })
    }

    pub async fn submit_observed(
        &self,
        binding: &SubmitRecordBinding,
        deadline: Instant,
    ) -> Result<bool> {
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        let output = port
            .program(
                &binding.frame,
                Program::SubmitRecordRead {
                    marker: binding.frame.marker.clone(),
                    submitter: binding.submitter,
                    record: binding.record,
                },
                deadline,
            )
            .await?;
        self.delivery(epoch, false)?;
        match output {
            Output::Applied(Some(submitted)) => Ok(submitted),
            Output::Applied(None) => Err(Failure::new("browser_not_actionable")),
            _ => Err(types::decode()),
        }
    }

    async fn submit_feedback_snapshot(
        &self,
        target: super::evidence::Target,
        anchor: &str,
        fields: bool,
        include_indicators: bool,
        deadline: Instant,
    ) -> Result<Output> {
        let deadline = deadline.min(Instant::now() + super::evidence::TIMEOUT);
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        let frames = port.frames(deadline).await?;
        let frame = frames
            .iter()
            .find(|f| f.context == target.context && f.parent.is_none())
            .ok_or_else(types::stale)?;
        if super::evidence::target(&frames, frame, target.node)? != target {
            return Err(types::stale());
        }
        let output = port
            .program(
                frame,
                Program::SubmitFeedbackRead {
                    marker: frame.marker.clone(),
                    record: target.node,
                    anchor_text: anchor.to_owned(),
                    fields,
                    include_indicators,
                },
                deadline,
            )
            .await?;
        self.delivery(epoch, false)?;
        Ok(output)
    }

    pub async fn read_submit_feedback(
        &self,
        target: super::evidence::Target,
        anchor: &str,
        deadline: Instant,
    ) -> Result<Option<SubmitFeedback>> {
        let Output::Feedback(feedback) = self
            .submit_feedback_snapshot(target, anchor, false, false, deadline)
            .await?
        else {
            return Err(types::decode());
        };
        Ok(feedback.map(|item| SubmitFeedback {
            kind: item.kind,
            field: item
                .field_node
                .map(|node| super::evidence::Target { node, ..target }),
            message: super::evidence::Target {
                node: item.message_node,
                ..target
            },
            reason: item.reason,
        }))
    }

    pub async fn read_pixel_reason(
        &self,
        target: super::evidence::Target,
        field: u64,
        source: u64,
        anchor: &str,
        deadline: Instant,
    ) -> Result<Option<super::program::PixelReason>> {
        let deadline = deadline.min(Instant::now() + super::evidence::TIMEOUT);
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        let frames = port.frames(deadline).await?;
        let frame = frames
            .iter()
            .find(|f| f.context == target.context && f.parent.is_none())
            .ok_or_else(types::stale)?;
        if super::evidence::target(&frames, frame, target.node)? != target {
            return Err(types::stale());
        }
        let output = port
            .program(
                frame,
                Program::SubmitPixelReasonRead {
                    marker: frame.marker.clone(),
                    record: target.node,
                    field,
                    source,
                    anchor_text: anchor.to_owned(),
                },
                deadline,
            )
            .await?;
        self.delivery(epoch, false)?;
        match output {
            Output::PixelReason(reason) => Ok(reason),
            _ => Err(types::decode()),
        }
    }

    pub async fn submit_pixel_slots(
        &self,
        target: super::evidence::Target,
        anchor: &str,
        deadline: Instant,
    ) -> Result<Vec<super::program::PixelSlot>> {
        let deadline = deadline.min(Instant::now() + super::evidence::TIMEOUT);
        let _operation = tokio::time::timeout_at(deadline.into(), self.0.operation.lock())
            .await
            .map_err(|_| Failure::new("browser_busy"))?;
        let (port, epoch) = self.authorization(false)?;
        let frames = port.frames(deadline).await?;
        let frame = frames
            .iter()
            .find(|f| f.context == target.context && f.parent.is_none())
            .ok_or_else(types::stale)?;
        if super::evidence::target(&frames, frame, target.node)? != target {
            return Err(types::stale());
        }
        let output = port
            .program(
                frame,
                Program::SubmitPixelSlotsRead {
                    marker: frame.marker.clone(),
                    record: target.node,
                    anchor_text: anchor.to_owned(),
                },
                deadline,
            )
            .await?;
        self.delivery(epoch, false)?;
        match output {
            Output::PixelSlots(slots) => Ok(slots),
            _ => Err(types::decode()),
        }
    }

    pub async fn read_submit_fields(
        &self,
        target: super::evidence::Target,
        anchor: &str,
        include_indicators: bool,
        deadline: Instant,
    ) -> Result<(
        Vec<SubmitField>,
        Vec<SubmitVisualField>,
        Vec<SubmitIndicator>,
    )> {
        let Output::Fields(reply) = self
            .submit_feedback_snapshot(target, anchor, true, include_indicators, deadline)
            .await?
        else {
            return Err(types::decode());
        };
        let fields = reply
            .fields
            .into_iter()
            .map(|item| SubmitField {
                field: super::evidence::Target {
                    node: item.field_node,
                    ..target
                },
                message: super::evidence::Target {
                    node: item.message_node,
                    ..target
                },
                reason: item.reason,
                association: item.association,
            })
            .collect::<Vec<_>>();
        let visuals = reply
            .visual_changes
            .into_iter()
            .map(|item| SubmitVisualField {
                field: super::evidence::Target {
                    node: item.field_node,
                    ..target
                },
                changes: item.changes,
            })
            .collect();
        let indicators = reply
            .indicators
            .unwrap_or_default()
            .into_iter()
            .map(|item| SubmitIndicator {
                field: super::evidence::Target {
                    node: item.field_node,
                    ..target
                },
                source: super::evidence::Target {
                    node: item.source_node,
                    ..target
                },
                kind: item.kind,
            })
            .collect();
        Ok((fields, visuals, indicators))
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
            if is_drag_endpoint(&element.facts) {
                return Err(Failure::new("browser_not_actionable"));
            }
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
        self.act_as(
            reference,
            action,
            true,
            deadline,
            ValidationProbe::Disabled,
            None,
        )
        .await
    }
    async fn act_as(
        &self,
        reference: Reference,
        action: Action,
        human: bool,
        deadline: Instant,
        submit_validation: ValidationProbe,
        submit_window: Option<task_events::Sink>,
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
            page.execute(
                reference,
                action,
                human,
                deadline,
                submit_validation,
                submit_window,
            )
            .await
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
        submit_validation: ValidationProbe,
        submit_window: Option<task_events::Sink>,
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
        let (element, target) = {
            let state = self.0.state.lock().unwrap();
            let element = state
                .refs
                .get(&reference)
                .cloned()
                .ok_or_else(types::stale)?;
            let target = match &action {
                Action::Drag { target } => {
                    if target.scope != self.0.scope
                        || target.epoch != epoch
                        || target.generation != reference.generation
                    {
                        return Err(types::stale());
                    }
                    Some(state.refs.get(target).cloned().ok_or_else(types::stale)?)
                }
                _ => None,
            };
            (element, target)
        };
        let (port, effects) = port.tracked();
        let result = perform(
            &port,
            &element,
            target.as_ref(),
            action,
            submit_validation,
            submit_window.as_ref(),
            deadline,
        )
        .await;
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
async fn prepare_input_target<P: Port>(
    port: &P,
    element: &Element,
    mut facts: Facts,
    kind: actionability::ActionKind,
    deadline: Instant,
) -> Result<Facts> {
    let top_level = element.frame.parent.is_none() && element.ancestors.is_empty();
    let marker = &element.frame.marker;
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
    Ok(steady)
}

async fn submit_probe_arm<P: Port>(
    port: &P,
    frame: &Frame,
    marker: &Marker,
    node: u64,
    enter: bool,
    register_field: bool,
    deadline: Instant,
) -> bool {
    matches!(
        port.program(
            frame,
            Program::SubmitProbeArm {
                marker: marker.clone(),
                node,
                enter,
                register_field,
            },
            deadline,
        )
        .await,
        Ok(Output::Applied(Some(true)))
    )
}

async fn submit_probe_read<P: Port>(
    port: &P,
    frame: &Frame,
    marker: &Marker,
    node: u64,
    deadline: Instant,
) -> Option<super::program::SubmitValidation> {
    match port
        .program(
            frame,
            Program::SubmitProbeRead {
                marker: marker.clone(),
                node,
            },
            deadline,
        )
        .await
    {
        Ok(Output::Probe(validation)) => validation,
        _ => None,
    }
}

async fn perform<P: Port>(
    port: &P,
    element: &Element,
    target: Option<&Element>,
    action: Action,
    submit_validation: ValidationProbe,
    submit_window: Option<&task_events::Sink>,
    deadline: Instant,
) -> Result<Acknowledgement> {
    if let Some(radio) = element.reuse_radio {
        if !matches!(action, Action::Click) {
            return Err(types::stale());
        }
        program::verify_reusable_submit(port, &element.frame, radio, element.facts.node, deadline)
            .await?;
    }
    if let Action::Drag { .. } = &action {
        let target = target.ok_or_else(types::stale)?;
        return perform_drag(port, element, target, deadline).await;
    }
    if is_drag_endpoint(&element.facts) {
        return Err(Failure::new("browser_not_actionable"));
    }
    let mut facts = inspect(port, element, deadline).await?;
    let kind = action.kind();
    let top_level = element.frame.parent.is_none() && element.ancestors.is_empty();
    if (facts.kind == "scrollable") != matches!(action, Action::Scroll { .. }) {
        return Err(Failure::new("browser_not_actionable"));
    }
    if facts.kind == "inferred"
        && !matches!(action, Action::Click | Action::Press(_) | Action::Hover)
    {
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
    if !matches!(action, Action::Scroll { .. }) {
        facts = prepare_input_target(port, element, facts, kind, deadline).await?;
    }
    let expected_check = match &action {
        Action::Check(value) => Some(*value),
        _ => None,
    };
    let mut native_validation = None;
    let path = match action {
        Action::Scroll { delta } => {
            let output = port
                .program(
                    &element.frame,
                    Program::ScrollTarget {
                        marker,
                        node,
                        delta,
                    },
                    deadline,
                )
                .await?;
            let Output::Applied(Some(changed)) = output else {
                return Err(Failure::new("browser_not_actionable"));
            };
            return Ok(ack(changed, "semantic_scroll"));
        }
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
        Action::Hover => {
            port.input(
                &element.frame,
                NativeInput::Hover { node: facts.node },
                deadline,
            )
            .await?;
            "native_hover"
        }
        Action::Click | Action::Check(_) => {
            let probe_armed = submit_validation != ValidationProbe::Disabled
                && submit_probe_arm(
                    port,
                    &element.frame,
                    &marker,
                    node,
                    false,
                    submit_validation == ValidationProbe::Field,
                    deadline,
                )
                .await;
            if let Some(window) = submit_window {
                window.submit_dispatch()?;
            }
            port.input(
                &element.frame,
                NativeInput::Click {
                    node: facts.node,
                    reuse_radio: element.reuse_radio,
                    x: facts.x,
                    y: facts.y,
                },
                deadline,
            )
            .await?;
            if probe_armed {
                native_validation =
                    submit_probe_read(port, &element.frame, &marker, node, deadline).await;
            }
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
            let probe_armed = submit_validation != ValidationProbe::Disabled
                && matches!(key, Key::Enter)
                && submit_probe_arm(
                    port,
                    &element.frame,
                    &marker,
                    node,
                    true,
                    submit_validation == ValidationProbe::Field,
                    deadline,
                )
                .await;
            port.input(&element.frame, NativeInput::Press(key), deadline)
                .await?;
            if probe_armed {
                native_validation =
                    submit_probe_read(port, &element.frame, &marker, node, deadline).await;
            }
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
        Action::Drag { .. } => unreachable!("drag handled before ordinary action dispatch"),
    };
    let mut acknowledgement = ack(true, path);
    if let Some(report) = native_validation {
        if submit_validation == ValidationProbe::Field
            && matches!(
                report.validation.state,
                super::evidence::ValidationState::Invalid
            )
            && let Some(node) = report.field_node
            && let Ok(frames) = port.frames(deadline).await
        {
            acknowledgement.native_validation_target =
                super::evidence::target(&frames, &element.frame, node).ok();
        }
        acknowledgement.native_validation = Some(report.validation);
    }
    Ok(acknowledgement)
}
fn is_drag_endpoint(facts: &Facts) -> bool {
    matches!(facts.kind.as_str(), "drag_source" | "drop_target")
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DragEndpoint {
    pub node: u64,
    pub fingerprint: [u8; 32],
    pub x: f64,
    pub y: f64,
}
impl DragEndpoint {
    fn from_element(element: &Element) -> Self {
        Self {
            node: element.facts.node,
            fingerprint: element.fingerprint,
            x: element.facts.x,
            y: element.facts.y,
        }
    }
}
pub(crate) async fn check_drag_endpoints<P: Port>(
    port: &P,
    frame: &Frame,
    source: DragEndpoint,
    target: DragEndpoint,
    deadline: Instant,
) -> Result<()> {
    if frame.parent.is_some() || source.node == 0 || target.node == 0 || source.node == target.node
    {
        return Err(types::stale());
    }
    same_document(frame, &port.refresh(frame, deadline).await?)?;
    let Output::Checks(values) = port
        .program(
            frame,
            Program::Recheck {
                marker: frame.marker.clone(),
                nodes: vec![source.node, target.node],
                endpoint: true,
            },
            deadline,
        )
        .await?
    else {
        return Err(types::decode());
    };
    let [Some(source_now), Some(target_now)] = values.as_slice() else {
        return Err(types::stale());
    };
    if source_now.node != source.node
        || target_now.node != target.node
        || source_now.fingerprint() != source.fingerprint
        || target_now.fingerprint() != target.fingerprint
        || source_now.x != source.x
        || source_now.y != source.y
        || target_now.x != target.x
        || target_now.y != target.y
        || source_now.kind != "drag_source"
        || target_now.kind != "drop_target"
        || !source_now.endpoint_eligible()
        || !target_now.endpoint_eligible()
        || !source_now.visible
        || !target_now.visible
        || !source_now.hit
        || !target_now.hit
        || !source_now.in_view
        || !target_now.in_view
    {
        return Err(types::stale());
    }
    Ok(())
}
async fn perform_drag<P: Port>(
    port: &P,
    source: &Element,
    target: &Element,
    deadline: Instant,
) -> Result<Acknowledgement> {
    if source.frame.context != target.frame.context
        || source.frame.document != target.frame.document
        || source.frame.realm != target.frame.realm
        || source.facts.kind != "drag_source"
        || target.facts.kind != "drop_target"
        || !source.facts.endpoint_eligible()
        || !target.facts.endpoint_eligible()
    {
        return Err(types::stale());
    }
    let source_endpoint = DragEndpoint::from_element(source);
    let target_endpoint = DragEndpoint::from_element(target);
    check_drag_endpoints(
        port,
        &source.frame,
        source_endpoint,
        target_endpoint,
        deadline,
    )
    .await?;
    port.input(
        &source.frame,
        NativeInput::Drag {
            source: source_endpoint,
            target: target_endpoint,
        },
        deadline,
    )
    .await?;
    Ok(ack(true, "native_drag"))
}

fn ack(changed: bool, path: &'static str) -> Acknowledgement {
    Acknowledgement {
        changed,
        input_acknowledged: true,
        business_result_verified: false,
        path,
        native_validation: None,
        native_validation_target: None,
    }
}
