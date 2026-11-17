//! Authenticated human leases and viewport input for the selected semantic backend.
use super::*;
use sem::{
    HumanPermit,
    program::{Output, Program},
    surface::HumanInput,
};
const LEASE: Duration = Duration::from_secs(120);
pub(super) struct Lease {
    owner: String,
    view: String,
    id: String,
    epoch: u64,
    until: Instant,
    sequence: u64,
    outcome: &'static str,
    permit: HumanPermit,
    frame: Option<Frame>,
}
struct Frame {
    id: String,
    at: Instant,
    tab: String,
    documents: Vec<sem::Frame>,
    focus: Option<(sem::Frame, Focus)>,
    dialog: Option<String>,
    width: f64,
    height: f64,
}
impl Lease {
    fn value(&self) -> Value {
        json!({"lease_id":self.id,"control_epoch":self.epoch,"last_sequence":self.sequence,"last_outcome":self.outcome,"lease_seconds":LEASE.as_secs()})
    }
    fn matches(&self, principal: &Principal, params: &Value, epoch: u64, paused: bool) -> bool {
        self.owner == principal.owner
            && params["view_id"] == self.view
            && Instant::now() < self.until
            && paused
            && self.epoch == epoch
            && params["lease_id"] == self.id
            && params["control_epoch"].as_u64() == Some(epoch)
    }
}
fn string<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or_else(|| Failure::new("browser_invalid_params"))
}
impl Browsers {
    pub(in crate::browser::runtime) async fn v2_human(
        &self,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
    ) -> Result<Value> {
        let id = string(params, "session_id")?;
        let slot = self
            .v2_sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.user == principal.user)
            .cloned()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        let action = string(params, "action")?;
        if action == "close" {
            return self
                .v2_control(id, &slot, &json!({"action":"close"}), deadline)
                .await;
        }
        let view = string(params, "view_id")?;
        let mut session = match slot.session.try_lock() {
            Ok(s) => s,
            Err(_) => {
                if action == "claim" {
                    slot.pause();
                    return Err(Failure::unknown("browser_input_pending"));
                }
                return Err(Failure::new("browser_busy"));
            }
        };
        let (epoch, paused) = slot.control.snapshot();
        if action == "status" {
            let mut value = slot.listing(id);
            if let Some(lease) = session
                .human
                .as_ref()
                .filter(|l| l.until > Instant::now() && l.epoch == epoch && paused)
            {
                value["controller"] = "human".into();
                if lease.owner == principal.owner && lease.view == view {
                    value["lease"] = lease.value();
                }
            }
            return Ok(value);
        }
        {
            let admission = self.admission.lock().unwrap();
            if !admission.online || admission.closing || slot.closing.load(Ordering::Acquire) {
                return Err(Failure::new("browser_paused"));
            }
        }
        let original = slot.port()?;
        if !original.connected() {
            slot.pause();
            return Err(Failure::unknown("browser_lost"));
        }
        let port = original.for_tab(&session.active).or_else(|_| {
            let tab = original
                .tabs()
                .into_iter()
                .find(|t| t.ready)
                .ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
            session.active = tab.tab_id.clone();
            *slot.active.lock().unwrap() = session.active.clone();
            original.for_tab(&tab.tab_id)
        })?;
        let page = sem::Page::new(port.clone());
        if action == "claim" {
            if let Some(lease) = session
                .human
                .as_ref()
                .filter(|l| l.until > Instant::now() && l.epoch == epoch && paused)
            {
                if lease.owner != principal.owner || lease.view != view {
                    return Err(Failure::new("browser_human_busy"));
                }
                page.check_human(&lease.permit)?;
            } else {
                let permit = page.claim_human(deadline).await?;
                let epoch = slot.control.snapshot().0;
                page.command(true, false, deadline, move |p| async move {
                    p.activate(deadline).await
                })
                .await?;
                session.human = Some(Lease {
                    owner: principal.owner.clone(),
                    view: view.into(),
                    id: token()?,
                    epoch,
                    until: Instant::now() + LEASE,
                    sequence: 0,
                    outcome: "none",
                    permit,
                    frame: None,
                });
            }
            slot.human_held.store(true, Ordering::Release);
            self.files.disclosure(id, false)?;
            session.invalidate();
            session.touched = Instant::now();
            return Ok(
                json!({"session_id":id,"controller":"human","profile_name":slot.profile_name,"selection":slot.selection,"lease":session.human.as_ref().unwrap().value()}),
            );
        }
        let lease = session
            .human
            .as_ref()
            .filter(|l| l.matches(principal, params, epoch, paused))
            .ok_or_else(|| Failure::new("browser_human_lease_expired"))?;
        page.check_human(&lease.permit)?;
        let deadline = deadline.min(lease.until);
        let dialog_recovery = (action == "screenshot"
            || action == "input" && params["input"]["kind"] == "dialog")
            && port.dialog().is_some();
        if !dialog_recovery && (port.pending() != 0 || page.resources().1 != 0) {
            return Err(Failure::unknown("browser_input_pending"));
        }
        if action.starts_with("upload_") {
            if !self.settings.uploads {
                return Err(Failure::new("browser_upload_disabled"));
            }
            let result = if action == "upload_begin" {
                if params["tab_id"] != session.active || port.dialog().is_some() {
                    return Err(Failure::new("browser_stale_frame"));
                }
                let (_, meta) = page.access(true)?.metadata(deadline).await?;
                let origin = crate::browser::page::origin(&meta.url)?;
                let state = slot.control.state.lock().unwrap();
                if *state != (epoch, true) {
                    return Err(Failure::new("browser_control_changed"));
                }
                self.files.upload_begin(
                    &files::Owner {
                        principal: Principal {
                            version: 1,
                            owner: slot.owner.clone(),
                            user: slot.user.clone(),
                            channel: "ai".into(),
                        },
                        session: id.into(),
                        tab: session.active.clone(),
                        origin,
                        control: slot.control.clone(),
                    },
                    principal,
                    &lease.id,
                    params,
                )
            } else {
                let state = slot.control.state.lock().unwrap();
                if *state != (epoch, true) {
                    return Err(Failure::new("browser_control_changed"));
                }
                self.files.upload_step(principal, id, &lease.id, params)
            }?;
            session.human.as_mut().unwrap().until = Instant::now() + LEASE;
            session.touched = Instant::now();
            return Ok(result);
        }
        if action == "release" {
            if params["share_page_with_ai"] != true {
                return Err(Failure::new("browser_disclosure_required"));
            }
            page.settled(deadline).await?;
            page.check_human(&lease.permit)?;
            let epoch = {
                let mut state = slot.control.state.lock().unwrap();
                if *state != (epoch, true) {
                    return Err(Failure::new("browser_control_changed"));
                }
                self.files.disclosure(id, true)?;
                state.0 += 1;
                state.1 = false;
                state.0
            };
            session.human = None;
            slot.human_held.store(false, Ordering::Release);
            session.invalidate();
            session.touched = Instant::now();
            return Ok(
                json!({"session_id":id,"controller":"ai","control_epoch":epoch,"new_observation_required":true}),
            );
        }
        if action == "screenshot" {
            let frame_id = token()?;
            let mut value;
            let frame = if let Some(dialog) = port.dialog() {
                value = json!({"kind":"dialog","data":"","mime_type":"image/jpeg","width":0,"height":0,"dialog":dialog,"text_input_available":false,
                    "page":{"tab_id":session.active}});
                Frame {
                    id: frame_id.clone(),
                    at: Instant::now(),
                    tab: session.active.clone(),
                    documents: vec![],
                    focus: None,
                    dialog: Some(dialog.dialog_id),
                    width: 0.,
                    height: 0.,
                }
            } else {
                let authorized = page.access(true)?;
                let (image, documents, _, meta) = capture(&authorized, deadline).await?;
                let mut focus = None;
                for document in &documents {
                    let Output::Surface(surface) = authorized
                        .program(document, Program::Surface, deadline)
                        .await?
                    else {
                        return Err(sem::types::decode());
                    };
                    if let Some(value) = surface.focus
                        && authorized.focused_frame(document, deadline).await?
                    {
                        if focus.is_some() {
                            return Err(Failure::new("browser_stale_frame"));
                        }
                        focus = Some((document.clone(), value));
                    }
                }
                if !same_frames(&documents, &authorized.frames(deadline).await?) {
                    return Err(Failure::new("browser_stale_frame"));
                }
                value = image;
                value["dialog"] = Value::Null;
                value["text_input_available"] = focus.is_some().into();
                value["tabs"] = serde_json::to_value(port.tabs()).unwrap();
                Frame {
                    id: frame_id.clone(),
                    at: Instant::now(),
                    tab: session.active.clone(),
                    documents,
                    focus,
                    dialog: None,
                    width: meta.width,
                    height: meta.height,
                }
            };
            if slot.control.snapshot() != (epoch, true) {
                return Err(Failure::new("browser_control_changed"));
            }
            let lease = session.human.as_mut().unwrap();
            lease.frame = Some(frame);
            lease.until = Instant::now() + LEASE;
            page.renew_human(&mut lease.permit)?;
            session.touched = Instant::now();
            value["session_id"] = id.into();
            value["control_epoch"] = epoch.into();
            value["frame_id"] = frame_id.into();
            return Ok(value);
        }
        if action != "input" {
            return Err(Failure::new("browser_invalid_params"));
        }
        let sequence = params["sequence"]
            .as_u64()
            .filter(|v| *v > 0 && *v < 9_007_199_254_740_991)
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        if sequence != lease.sequence + 1 {
            return Err(Failure::new("browser_duplicate_input"));
        }
        let input = &params["input"];
        super::super::human::validate_input(input)?;
        let frame = lease
            .frame
            .as_ref()
            .filter(|f| {
                params["frame_id"] == f.id
                    && f.tab == session.active
                    && f.at.elapsed()
                        < if input["kind"] == "text" {
                            LEASE
                        } else {
                            Duration::from_secs(10)
                        }
            })
            .ok_or_else(|| Failure::new("browser_stale_frame"))?;
        let kind = input["kind"].as_str().unwrap();
        if kind == "dialog" {
            if frame.dialog.as_deref() != input["dialog_id"].as_str()
                || port
                    .dialog()
                    .as_ref()
                    .is_none_or(|d| input["dialog_id"] != d.dialog_id)
            {
                return Err(Failure::new("browser_stale_frame"));
            }
        } else if frame.dialog.is_some()
            || port.dialog().is_some()
            || !same_frames(
                &frame.documents,
                &page.access(true)?.frames(deadline).await?,
            )
        {
            return Err(Failure::new("browser_stale_frame"));
        }
        if matches!(kind, "click" | "scroll")
            && (input["x"].as_f64().unwrap() >= frame.width
                || input["y"].as_f64().unwrap() >= frame.height)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        let focus = frame.focus.clone();
        let lease = session.human.as_mut().unwrap();
        lease.sequence = sequence;
        lease.outcome = "unknown";
        lease.frame = None;
        let result = match kind {
            "dialog" => {
                let dialog = input["dialog_id"].as_str().unwrap().to_owned();
                let accept = input["accept"].as_bool().unwrap();
                page.command(true, true, deadline, move |p| async move {
                    p.handle_dialog(&dialog, accept, deadline).await
                })
                .await
            }
            "tab" => {
                let tab = input["tab_id"].as_str().unwrap().to_owned();
                let target = port.for_tab(&tab)?;
                let page = sem::Page::new(target);
                let result = page
                    .command(true, false, deadline, move |p| async move {
                        p.activate(deadline).await
                    })
                    .await;
                if result.is_ok() {
                    session.active = tab;
                    *slot.active.lock().unwrap() = session.active.clone();
                }
                result
            }
            _ => {
                let native = match kind {
                    "click" => HumanInput::Click {
                        x: input["x"].as_f64().unwrap(),
                        y: input["y"].as_f64().unwrap(),
                    },
                    "scroll" => HumanInput::Scroll {
                        x: input["x"].as_f64().unwrap(),
                        y: input["y"].as_f64().unwrap(),
                        delta: input["delta_y"].as_f64().unwrap(),
                    },
                    "key" => HumanInput::Key(sem::Key::parse(input["key"].as_str().unwrap())?),
                    "text" => {
                        let (frame, focus) =
                            focus.ok_or_else(|| Failure::new("browser_not_actionable"))?;
                        HumanInput::Text {
                            frame: Box::new(frame),
                            focus,
                            text: input["text"].as_str().unwrap().into(),
                        }
                    }
                    _ => return Err(Failure::new("browser_invalid_params")),
                };
                page.command(true, false, deadline, move |p| async move {
                    p.human_input(native, deadline).await
                })
                .await
            }
        };
        let lease = session.human.as_mut().unwrap();
        if result.is_ok() {
            lease.outcome = "acknowledged";
        }
        lease.until = Instant::now() + LEASE;
        page.renew_human(&mut lease.permit)?;
        session.touched = Instant::now();
        result?;
        Ok(
            json!({"session_id":id,"sequence":sequence,"input_acknowledged":true,"business_result_verified":false}),
        )
    }
}
