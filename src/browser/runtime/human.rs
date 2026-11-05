//! Authenticated, browser-only human control. Secrets are transient input, never receipts.
use super::*;
mod text;

struct HumanFrame {
    id: String,
    at: Instant,
    stamp: String,
    focus: Option<text::Focus>,
    dialog: Option<String>,
}

const LEASE: Duration = Duration::from_secs(120);
pub(super) struct HumanLease {
    owner: String,
    view: String,
    id: String,
    epoch: u64,
    until: Instant,
    sequence: u64,
    outcome: &'static str,
    frame: Option<HumanFrame>,
}
impl HumanLease {
    fn value(&self) -> Value {
        json!({"lease_id":self.id,"control_epoch":self.epoch,"last_sequence":self.sequence,
            "last_outcome":self.outcome,"lease_seconds":LEASE.as_secs()})
    }
}
fn string<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or_else(|| Failure::new("browser_invalid_params"))
}
impl Browsers {
    pub(super) async fn human(
        &self,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
    ) -> Result<Value> {
        self.prepare()?;
        let action = string(params, "action")?;
        let keys: &[&str] = match action {
            "list" => &["action"],
            "close" => &["action", "session_id"],
            "status" | "claim" => &["action", "session_id", "view_id"],
            "screenshot" => &[
                "action",
                "session_id",
                "view_id",
                "lease_id",
                "control_epoch",
            ],
            "release" => &[
                "action",
                "session_id",
                "view_id",
                "lease_id",
                "control_epoch",
                "share_page_with_ai",
            ],
            "input" => &[
                "action",
                "session_id",
                "view_id",
                "lease_id",
                "control_epoch",
                "sequence",
                "frame_id",
                "input",
            ],
            _ => return Err(Failure::new("browser_invalid_params")),
        };
        if !params
            .as_object()
            .is_some_and(|m| m.len() == keys.len() && m.keys().all(|k| keys.contains(&k.as_str())))
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if action == "list" {
            let slots: Vec<_> = self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, s)| s.user == principal.user)
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect();
            let sessions: Vec<_>=slots.iter().map(|(id,slot)| {
                let (epoch,paused)=slot.control.snapshot();
                json!({"session_id":id,"control_epoch":epoch,"controller":if paused{"paused"}else{"ai"}})
            }).collect();
            return Ok(json!({"sessions":sessions}));
        }
        let id = string(params, "session_id")?;
        let slot = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.user == principal.user)
            .cloned()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        if action == "close" {
            return self
                .control(id, &slot, &json!({"action":"close"}), deadline)
                .await;
        }
        let view = string(params, "view_id")?;
        let mut session = match slot.session.try_lock() {
            Ok(session) => session,
            Err(_) => {
                if action == "claim" {
                    if !slot.control.snapshot().1 {
                        slot.control.change(true);
                    }
                    return Err(Failure::unknown("browser_input_pending"));
                }
                return Err(Failure::new("browser_busy"));
            }
        };
        if action == "claim" {
            // Keep the action lock from fencing through lease installation; AI
            // resume must not slip between those two steps.
            if let Some(human) = session.human.as_ref().filter(|h| h.until > Instant::now())
                && (human.owner != principal.owner || human.view != view)
            {
                return Err(Failure::new("browser_human_busy"));
            }
            if session
                .human
                .as_ref()
                .is_none_or(|h| slot.control.snapshot() != (h.epoch, true))
            {
                slot.control.change(true);
            }
        }
        let (epoch, paused) = slot.control.snapshot();
        if action == "status" {
            let mut value = json!({"session_id":id,"control_epoch":epoch,"controller":if paused{"paused"}else{"ai"}});
            if let Some(human) = session
                .human
                .as_ref()
                .filter(|h| h.until > Instant::now() && h.epoch == epoch && paused)
            {
                value["controller"] = "human".into();
                if human.owner == principal.owner && human.view == view {
                    value["lease"] = human.value();
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
        let process = session
            .process
            .as_ref()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        if !process.cdp.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        let dialog_recovery = (action == "screenshot"
            || action == "input" && params["input"]["kind"] == "dialog")
            && session
                .targets
                .lock()
                .unwrap()
                .get(&session.active)
                .is_some_and(|t| t.dialog.is_some());
        if process.cdp.pending() != 0 && !dialog_recovery {
            return Err(Failure::unknown("browser_input_pending"));
        }
        let cdp = process.cdp.human(slot.control.clone(), epoch);
        if action == "claim" {
            if let Some(human) = &session.human
                && human.epoch == epoch
                && paused
                && human.until > Instant::now()
                && (human.owner != principal.owner || human.view != view)
            {
                return Err(Failure::new("browser_human_busy"));
            }
            if session
                .human
                .as_ref()
                .is_none_or(|h| h.epoch != epoch || h.until <= Instant::now())
            {
                session.human = Some(HumanLease {
                    owner: principal.owner.clone(),
                    view: view.into(),
                    id: crate::random::hex_token::<16>().map_err(super::super::io_error)?,
                    epoch,
                    until: Instant::now() + LEASE,
                    sequence: 0,
                    outcome: "none",
                    frame: None,
                });
            }
            session.references.clear();
            session.touched = Instant::now();
            if slot.control.snapshot() != (epoch, true) {
                return Err(Failure::new("browser_control_changed"));
            }
            return Ok(
                json!({"session_id":id,"controller":"human","lease":session.human.as_ref().unwrap().value()}),
            );
        }
        let human = session
            .human
            .as_ref()
            .filter(|h| {
                h.owner == principal.owner
                    && h.view == view
                    && h.until > Instant::now()
                    && h.epoch == epoch
                    && paused
                    && params["lease_id"] == h.id
                    && params["control_epoch"].as_u64() == Some(epoch)
            })
            .ok_or_else(|| Failure::new("browser_human_lease_expired"))?;
        let deadline = deadline.min(human.until);
        if action == "release" {
            if params["share_page_with_ai"] != true {
                return Err(Failure::new("browser_invalid_params"));
            }
            let epoch = slot.control.resume(epoch)?;
            session.human = None;
            session.references.clear();
            session.touched = Instant::now();
            return Ok(
                json!({"session_id":id,"controller":"ai","control_epoch":epoch,"new_observation_required":true}),
            );
        }
        let target = {
            let targets = session.targets.lock().unwrap();
            targets
                .get(&session.active)
                .cloned()
                .or_else(|| {
                    (action == "screenshot")
                        .then(|| {
                            targets
                                .values()
                                .filter(|t| t.kind == "page")
                                .min_by_key(|t| &t.id)
                                .cloned()
                        })
                        .flatten()
                })
                .ok_or_else(|| Failure::new("browser_tab_unavailable"))?
        };
        if action == "screenshot" && session.active != target.id {
            cdp.call(
                Some(&target.session),
                "Page.bringToFront",
                json!({}),
                deadline,
            )
            .await?;
            session.active = target.id.clone();
        }
        let target_session = Some(target.session.as_str());
        if action == "screenshot" {
            // Chromium suspends even Page.getFrameTree and captureScreenshot
            // while a native JavaScript dialog waits. Use the dialog event only.
            if let Some(dialog) = &target.dialog {
                let frame_id = crate::random::hex_token::<12>().map_err(super::super::io_error)?;
                let human = session.human.as_mut().unwrap();
                human.frame = Some(HumanFrame {
                    id: frame_id.clone(),
                    at: Instant::now(),
                    stamp: String::new(),
                    focus: None,
                    dialog: dialog["dialog_id"].as_str().map(str::to_owned),
                });
                human.until = Instant::now() + LEASE;
                session.touched = Instant::now();
                return Ok(
                    json!({"session_id":id,"control_epoch":epoch,"frame_id":frame_id,"kind":"dialog",
                    "mime_type":"image/jpeg","data":"","width":0,"height":0,"dialog":dialog,"text_input_available":false,
                    "page":{"url":target.url,"title":target.title,"tab_id":target.id}}),
                );
            }
            let stamp = frame_stamp(&cdp, &session, &target, deadline).await?;
            let focus = if target.dialog.is_some() {
                None
            } else {
                text::capture(
                    &cdp,
                    &frames(&cdp, &session, &target, deadline).await?,
                    deadline,
                )
                .await?
            };
            let image = cdp
                .call(
                    target_session,
                    "Page.captureScreenshot",
                    json!({"format":"jpeg","quality":60,"captureBeyondViewport":false}),
                    deadline,
                )
                .await?;
            let data = image["data"]
                .as_str()
                .filter(|s| s.len() <= 750000)
                .ok_or_else(|| Failure::new("browser_image_too_large"))?;
            let metrics = cdp
                .call(target_session, "Page.getLayoutMetrics", json!({}), deadline)
                .await?;
            if slot.control.snapshot() != (epoch, true)
                || frame_stamp(&cdp, &session, &target, deadline).await? != stamp
            {
                return Err(Failure::new("browser_control_changed"));
            }
            let frame_id = crate::random::hex_token::<12>().map_err(super::super::io_error)?;
            let human = session.human.as_mut().unwrap();
            let text_input_available = focus.is_some();
            human.frame = Some(HumanFrame {
                id: frame_id.clone(),
                at: Instant::now(),
                stamp,
                focus,
                dialog: None,
            });
            human.until = Instant::now() + LEASE;
            session.touched = Instant::now();
            let tabs: Vec<_> = session
                .targets
                .lock()
                .unwrap()
                .values()
                .filter(|t| t.kind == "page")
                .map(|t| json!({"tab_id":t.id,"title":t.title,"url":t.url}))
                .collect();
            return Ok(
                json!({"session_id":id,"control_epoch":epoch,"frame_id":frame_id,"mime_type":"image/jpeg","data":data,
                "width":metrics["cssLayoutViewport"]["clientWidth"],"height":metrics["cssLayoutViewport"]["clientHeight"],
                "page":{"url":target.url,"title":target.title,"tab_id":target.id},"tabs":tabs,"dialog":target.dialog,"text_input_available":text_input_available}),
            );
        }
        let human = session.human.as_ref().unwrap();
        let sequence = params["sequence"]
            .as_u64()
            .filter(|n| *n > 0 && *n < 9_007_199_254_740_991)
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        if sequence != human.sequence + 1 {
            return Err(Failure::new("browser_duplicate_input"));
        }
        let input = &params["input"];
        validate_input(input)?;
        if !human.frame.as_ref().is_some_and(|frame| {
            params["frame_id"] == frame.id
                && frame.at.elapsed()
                    < if input["kind"] == "text" {
                        LEASE
                    } else {
                        Duration::from_secs(10)
                    }
        }) {
            return Err(Failure::new("browser_stale_frame"));
        }
        if input["kind"] == "dialog" {
            if human.frame.as_ref().unwrap().dialog.as_deref() != input["dialog_id"].as_str()
                || target
                    .dialog
                    .as_ref()
                    .is_none_or(|d| d["dialog_id"] != input["dialog_id"])
            {
                return Err(Failure::new("browser_stale_frame"));
            }
        } else if human.frame.as_ref().unwrap().dialog.is_some()
            || frame_stamp(&cdp, &session, &target, deadline).await?
                != human.frame.as_ref().unwrap().stamp
        {
            return Err(Failure::new("browser_stale_frame"));
        }
        if matches!(input["kind"].as_str(), Some("click" | "scroll")) {
            let metrics = cdp
                .call(target_session, "Page.getLayoutMetrics", json!({}), deadline)
                .await?;
            if input["x"].as_f64().unwrap()
                >= metrics["cssLayoutViewport"]["clientWidth"]
                    .as_f64()
                    .unwrap_or(0.)
                || input["y"].as_f64().unwrap()
                    >= metrics["cssLayoutViewport"]["clientHeight"]
                        .as_f64()
                        .unwrap_or(0.)
            {
                return Err(Failure::new("browser_invalid_params"));
            }
        }
        session.human.as_mut().unwrap().sequence = sequence;
        session.human.as_mut().unwrap().outcome = "unknown";
        let frame = session.human.as_mut().unwrap().frame.take().unwrap();
        let outcome = if input["kind"] == "dialog" {
            handle_dialog(
                &cdp,
                &session.targets,
                &target,
                &input["dialog_id"],
                &input["accept"],
                deadline,
            )
            .await
        } else if input["kind"] == "tab" {
            let tab = input["tab_id"].as_str().unwrap();
            let chosen = session
                .targets
                .lock()
                .unwrap()
                .get(tab)
                .filter(|t| t.kind == "page")
                .cloned();
            if let Some(chosen) = chosen {
                match cdp
                    .call(
                        Some(&chosen.session),
                        "Page.bringToFront",
                        json!({}),
                        deadline,
                    )
                    .await
                {
                    Ok(_) => {
                        session.active = tab.into();
                        session.references.clear();
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            } else {
                Err(Failure::new("browser_tab_unavailable"))
            }
        } else if input["kind"] == "text" {
            match frame.focus {
                Some(focus) => focus.insert(&cdp, &input["text"], deadline).await,
                None => Err(Failure::new("browser_not_actionable")),
            }
        } else {
            send_input(&cdp, target_session, input, deadline).await
        };
        if outcome.is_ok() {
            session.human.as_mut().unwrap().outcome = "acknowledged";
        }
        session.human.as_mut().unwrap().until = Instant::now() + LEASE;
        session.touched = Instant::now();
        outcome?;
        if slot.control.snapshot() != (epoch, true) {
            return Err(Failure::unknown("browser_control_changed"));
        }
        Ok(
            json!({"session_id":id,"sequence":sequence,"input_acknowledged":true,"business_result_verified":false}),
        )
    }
}
fn validate_input(input: &Value) -> Result<()> {
    let fields: &[&str] = match input["kind"].as_str().unwrap_or("") {
        "click" => &["kind", "x", "y"],
        "scroll" => &["kind", "x", "y", "delta_y"],
        "text" => &["kind", "text"],
        "key" => &["kind", "key"],
        "dialog" => &["kind", "accept", "dialog_id"],
        "tab" => &["kind", "tab_id"],
        _ => return Err(Failure::new("browser_invalid_params")),
    };
    if !input
        .as_object()
        .is_some_and(|m| m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str())))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    for key in ["x", "y"] {
        if fields.contains(&key)
            && !input[key]
                .as_f64()
                .is_some_and(|n| n.is_finite() && (0.0..=4096.0).contains(&n))
        {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    if fields.contains(&"delta_y")
        && !input["delta_y"]
            .as_f64()
            .is_some_and(|n| n.is_finite() && n.abs() <= 2000.)
        || fields.contains(&"text") && input["text"].as_str().is_none_or(|s| s.len() > 16384)
        || fields.contains(&"accept") && !input["accept"].is_boolean()
        || fields.contains(&"key") && input["key"].as_str().is_none_or(|s| key_code(s).is_none())
        || fields.contains(&"tab_id") && string(input, "tab_id").is_err()
        || fields.contains(&"dialog_id") && string(input, "dialog_id").is_err()
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    Ok(())
}
fn key_code(key: &str) -> Option<i32> {
    Some(match key {
        "Enter" => 13,
        "Tab" => 9,
        "Escape" => 27,
        "Backspace" => 8,
        "ArrowDown" => 40,
        "ArrowUp" => 38,
        "ArrowLeft" => 37,
        "ArrowRight" => 39,
        _ => return None,
    })
}
async fn send_input(
    cdp: &Cdp,
    session: Option<&str>,
    input: &Value,
    deadline: Instant,
) -> Result<()> {
    match input["kind"].as_str().unwrap() {
        "click" => {
            cdp.input_pair(session,"Input.dispatchMouseEvent",json!({"type":"mousePressed","x":input["x"],"y":input["y"],"button":"left","clickCount":1}),json!({"type":"mouseReleased","x":input["x"],"y":input["y"],"button":"left","clickCount":1}),deadline).await?;
        }
        "scroll" => {
            cdp.call(session,"Input.dispatchMouseEvent",json!({"type":"mouseWheel","x":input["x"],"y":input["y"],"deltaX":0,"deltaY":input["delta_y"]}),deadline).await?;
        }
        "key" => {
            let key = input["key"].as_str().unwrap();
            let code = key_code(key).unwrap();
            cdp.input_pair(session,"Input.dispatchKeyEvent",json!({"type":"keyDown","key":key,"windowsVirtualKeyCode":code,"text":if key=="Enter"{"\r"}else{""}}),json!({"type":"keyUp","key":key,"windowsVirtualKeyCode":code}),deadline).await?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

async fn frame_stamp(
    cdp: &Cdp,
    session: &Session,
    target: &Target,
    deadline: Instant,
) -> Result<String> {
    let mut identities: Vec<_> = frames(cdp, session, target, deadline)
        .await?
        .into_iter()
        .map(|frame| (frame.session, frame.id, frame.loader))
        .collect();
    identities.sort();
    Ok(crate::config::sha256_hex(
        &serde_json::to_vec(&json!([
            identities,
            session
                .targets
                .lock()
                .unwrap()
                .get(&target.id)
                .and_then(|t| t.dialog.as_ref())
                .map(|d| &d["dialog_id"])
        ]))
        .unwrap(),
    ))
}
