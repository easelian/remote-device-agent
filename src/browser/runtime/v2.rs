//! Explicit capability v2 sessions. Legacy v1 algorithms remain in runtime.rs.
use super::*;
use crate::browser::{
    selection::{Backend, Selection},
    semantic::{self as sem, Port as _},
};
use sem::surface::{Focus, Metadata};
mod human;
mod native;

pub(super) struct Slot {
    owner: String,
    user: String,
    selection: Selection,
    profile_name: Option<String>,
    coordination: Arc<sem::Coordination>,
    control: Arc<Control>,
    closing: AtomicBool,
    human_held: AtomicBool,
    active: Mutex<String>,
    port: Mutex<Option<native::Port>>,
    session: AsyncMutex<Session>,
    _storage_lock: Arc<crate::profile_lock::ProfileLocks>,
}
struct Session {
    native: Option<native::Session>,
    active: String,
    references: HashMap<String, sem::Reference>,
    reference_tab: Option<String>,
    frames: HashMap<String, sem::Context>,
    cursor: Option<(String, sem::Continuation, sem::Query)>,
    human: Option<human::Lease>,
    touched: Instant,
}
impl Session {
    fn invalidate(&mut self) {
        self.references.clear();
        self.reference_tab = None;
        self.frames.clear();
        self.cursor = None;
    }
    async fn close(&mut self) -> Result<()> {
        if let Some(native) = &mut self.native {
            native.close().await?;
        }
        self.native = None;
        self.invalidate();
        self.human = None;
        Ok(())
    }
    fn reference(&self, id: &str, tab: &str) -> Result<sem::Reference> {
        if self.reference_tab.as_deref() != Some(tab) {
            return Err(Failure::new("browser_stale_reference"));
        }
        self.references
            .get(id)
            .cloned()
            .ok_or_else(|| Failure::new("browser_stale_reference"))
    }
}
impl Slot {
    fn port(&self) -> Result<native::Port> {
        self.port
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Failure::new("browser_busy"))
    }
    fn pause(&self) {
        self.coordination.pause();
    }
    fn stop(&self) {
        self.pause();
        self.closing.store(true, Ordering::Release);
        // Fence new input immediately; native close must retain its graceful
        // transport path so named browser databases can flush before exit.
    }
    fn listing(&self, id: &str) -> Value {
        let (epoch, paused) = self.control.snapshot();
        let state = if self.closing.load(Ordering::Acquire) {
            "closing"
        } else {
            match self.port() {
                Ok(p) if p.connected() => "ready",
                Ok(_) => "lost",
                Err(_) => "opening",
            }
        };
        json!({"session_id":id,"selection":self.selection,"state":state,"control_epoch":epoch,"controller":if paused{"paused"}else{"ai"},"profile_name":self.profile_name})
    }
}
fn token() -> Result<String> {
    crate::random::hex_token::<16>().map_err(crate::browser::io_error)
}
fn capability(settings: &Settings, backend: Backend) -> Value {
    json!({"managed_lifecycle":true,"document_identity":true,"observation":true,"input":true,"screenshot":true,
        "dialogs":true,"human_takeover":true,"navigation_policy":true,"conservative_recovery":true,
        "download_cancellation":if backend==Backend::Bidi{"session"}else{"file"},"max_tabs":8,"max_frames":32,"named_profiles":settings.persistent_profiles,"uploads":settings.uploads,"downloads":settings.downloads,"native_ax":false,"devtools":false,
        "human_text":"focused_native_text_controls","network_isolation":false,"observed_connection_address":false,
        "actions":["attach_file","navigate","click","fill","press","select","check","scroll","dialog","new_tab","close_tab","select_tab"],
        "fill":"native_text_edit","select":"validated_native_form_value","scroll_scope":"main_document",
        "extensions":{"actionability":crate::browser::selection::actionability(None)}})
}
fn same_frames(a: &[sem::Frame], b: &[sem::Frame]) -> bool {
    a.len() == b.len() && a.iter().all(|f| b.contains(f))
}
async fn capture(
    port: &native::Port,
    deadline: Instant,
) -> Result<(Value, Vec<sem::Frame>, sem::Frame, Metadata)> {
    let before = port.frames(deadline).await?;
    let (frame, meta) = port.metadata(deadline).await?;
    let mut image = port.screenshot(deadline).await?;
    if !same_frames(&before, &port.frames(deadline).await?) {
        return Err(Failure::new("browser_stale_frame"));
    }
    image["page"] = meta.public();
    image["page"]["tab_id"] = port.tab_id()?.into();
    Ok((image, before, frame, meta))
}
impl Browsers {
    pub(super) fn v2_listing(&self, principal: &Principal, human: bool) -> Vec<Value> {
        self.v2_sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| {
                if human {
                    s.user == principal.user
                } else {
                    s.owner == principal.owner
                }
            })
            .map(|(id, s)| {
                let mut value = s.listing(id);
                value["capabilities"] = capability(&self.settings, s.selection.backend);
                value
            })
            .collect()
    }
    pub(super) fn v2_contains(&self, params: &Value) -> bool {
        params["session_id"]
            .as_str()
            .is_some_and(|id| self.v2_sessions.lock().unwrap().contains_key(id))
    }
    pub(super) async fn v2_open(
        &self,
        params: &Value,
        principal: &Principal,
        selection: Selection,
        deadline: Instant,
    ) -> Result<Value> {
        let info = self.info();
        if !info["v2"]["backends"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|b| b["id"] == selection.backend.id() && b["available"] == true)
        }) {
            return Err(Failure::new("browser_backend_unavailable"));
        }
        let profile_name = params["profile_name"].as_str();
        if profile_name.is_some() && !self.settings.persistent_profiles {
            return Err(Failure::new("browser_profile_disabled"));
        }
        self.admit(&params["url"])?;
        self.prepare_storage()?;
        let root = crate::browser::process::prepare_v2(
            self.root.as_deref().unwrap(),
            selection.backend.id(),
        )?;
        let id = token()?;
        let coordination = Arc::new(sem::Coordination::new());
        let slot = Arc::new(Slot {
            owner: principal.owner.clone(),
            user: principal.user.clone(),
            selection: selection.clone(),
            profile_name: profile_name.map(str::to_owned),
            control: coordination.control(),
            coordination: coordination.clone(),
            closing: AtomicBool::new(false),
            human_held: AtomicBool::new(false),
            active: Mutex::new(String::new()),
            port: Mutex::new(None),
            session: AsyncMutex::new(Session {
                native: None,
                active: String::new(),
                references: HashMap::new(),
                reference_tab: None,
                frames: HashMap::new(),
                cursor: None,
                human: None,
                touched: Instant::now(),
            }),
            _storage_lock: self.root_lock.lock().unwrap().as_ref().unwrap().clone(),
        });
        {
            let admission = self.admission.lock().unwrap();
            if !admission.online || admission.closing {
                return Err(Failure::new("browser_paused"));
            }
            let legacy = self.sessions.lock().unwrap();
            let mut sessions = self.v2_sessions.lock().unwrap();
            if legacy.len() + sessions.len() >= MAX_SESSIONS {
                return Err(Failure::new("browser_session_limit"));
            }
            sessions.insert(id.clone(), slot.clone());
        }
        let mut session = slot.session.lock().await;
        let result=async {
            let policy=sem::policy::Policy::new(self.settings.sites.clone());
            let named=profile_name.map(|name|self.profiles.open_scoped(&root,principal,name,selection.backend.profile_scope())).transpose()?;
            let data=process::SessionData {named,downloads:self.settings.downloads.then(||files::Downloads{files:self.files.clone(),principal:principal.clone(),session:id.clone(),control:slot.control.clone()})};
            let native=match selection.backend {
                Backend::Cdp=>native::Session::Cdp(crate::browser::engine::semantic_cdp::Session::launch_named(self.executable.as_deref().unwrap(),&root,&id,policy,coordination.clone(),data,deadline).await?),
                Backend::Bidi=>native::Session::Bidi(crate::browser::engine::bidi::page::Session::open_named(&std::env::current_exe().map_err(crate::browser::io_error)?,self.settings.firefox_executable.as_deref().unwrap(),&root,policy,coordination,data).await?),
            };
            let port=native.port();session.native=Some(native);*slot.port.lock().unwrap()=Some(port.clone());
            if slot.closing.load(Ordering::Acquire)||slot.control.snapshot()!=(1,false){return Err(Failure::new("browser_control_changed"));}
            let page=sem::Page::new(port.clone());
            // Handshake includes a real document stamp and bounded native JPEG.
            // No external navigation happens until the required surface works.
            capture(&page.access(false)?,deadline).await?;
            if let Some(url)=params["url"].as_str(){let url=url.to_owned();page.command(false,false,deadline,move|p|async move{p.navigate(&url,deadline).await}).await?;}
            if slot.control.snapshot()!=(1,false){return Err(Failure::unknown("browser_control_changed"));}
            session.active=port.tab_id()?;
            *slot.active.lock().unwrap()=session.active.clone();
            Ok(json!({"session_id":id,"tab_id":session.active,"control_epoch":1,"state":"ready","profile":if profile_name.is_some(){"named"}else{"temporary"},"profile_name":profile_name,"profile_id":profile_name.map(|n|selection.backend.profile_scope().id(principal,n)),
                "selection":selection,"capabilities":capability(&self.settings,selection.backend),"stage":"session_ready"}))
        }.await;
        if result.is_err() {
            slot.stop();
            if session.close().await.is_ok() {
                self.v2_sessions.lock().unwrap().remove(&id);
            }
        }
        drop(session);
        if result.is_ok() {
            let weak = Arc::downgrade(&slot);
            let all = Arc::downgrade(&self.v2_sessions);
            let id = id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let (Some(slot), Some(all)) = (weak.upgrade(), all.upgrade()) else {
                        break;
                    };
                    if let Ok(mut s) = slot.session.try_lock() {
                        if s.native.is_none() {
                            break;
                        }
                        if s.touched.elapsed() >= IDLE {
                            slot.stop();
                            if s.close().await.is_ok() {
                                all.lock().unwrap().remove(&id);
                                break;
                            }
                        }
                    }
                }
            });
        }
        result
    }
    pub(super) async fn v2_execute(
        &self,
        operation: &str,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
        operation_id: Option<&str>,
    ) -> Result<Value> {
        let id = params["session_id"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let slot = self
            .v2_sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.owner == principal.owner)
            .cloned()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        if operation == "browser_control" {
            return self.v2_control(id, &slot, params, deadline).await;
        }
        let (epoch, paused) = slot.control.snapshot();
        let port = slot.port()?;
        if slot.human_held.load(Ordering::Acquire) {
            return Err(Failure::new("browser_paused"));
        }
        if !port.connected() {
            slot.pause();
            return Err(Failure::unknown("browser_lost"));
        }
        if operation == "browser_tabs" {
            let value = json!({"session_id":id,"control_epoch":epoch,"active_tab":slot.active.lock().unwrap().clone(),"tabs":port.tabs(),"selection":slot.selection,
                "controller":if paused{"paused"}else{"ai"},"input_pending":port.pending()!=0});
            if slot.control.snapshot() != (epoch, paused) {
                return Err(Failure::new("browser_control_changed"));
            }
            return Ok(value);
        }
        let dialog = operation == "browser_act" && params["action"]["kind"] == "dialog";
        if slot.closing.load(Ordering::Acquire) || paused && !dialog {
            return Err(Failure::new("browser_paused"));
        }
        if params["control_epoch"].as_u64() != Some(epoch) {
            return Err(Failure::new("browser_control_changed"));
        }
        // A modal dialog may hold the originating input command open. Its
        // explicit recovery uses the emergency lane without that command's lock.
        if dialog {
            let tab = params["tab_id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| slot.active.lock().unwrap().clone());
            let page = sem::Page::new(port.for_tab(&tab)?);
            let dialog_id = params["action"]["dialog_id"].as_str().unwrap().to_owned();
            let accept = params["action"]["accept"].as_bool().unwrap();
            page.command(paused, true, deadline, move |p| async move {
                p.handle_dialog(&dialog_id, accept, deadline).await
            })
            .await?;
            return Ok(
                json!({"session_id":id,"control_epoch":epoch,"selection":slot.selection,"input_acknowledged":true,"business_result_verified":false}),
            );
        }
        let mut session = slot
            .session
            .try_lock()
            .map_err(|_| Failure::new("browser_busy"))?;
        if session.human.is_some() {
            return Err(Failure::new("browser_paused"));
        }
        session.touched = Instant::now();
        let tab = params["tab_id"]
            .as_str()
            .unwrap_or(&session.active)
            .to_owned();
        let port = port.for_tab(&tab)?;
        let page = sem::Page::new(port.clone());
        if !dialog && port.dialog().is_some() {
            return Err(Failure::new("browser_dialog_open"));
        }
        if !dialog && (port.pending() != 0 || page.resources().1 != 0) {
            return Err(Failure::unknown("browser_input_pending"));
        }
        let result = async {
            match operation {
                "browser_observe" => observe(&mut session, &page, &port, params, deadline).await,
                "browser_inspect" => {
                    if params["action_kind"] == "attach_file" && !self.settings.uploads {
                        return Err(Failure::new("browser_upload_disabled"));
                    }
                    let reference = session.reference(params["ref"].as_str().unwrap(), &tab)?;
                    let kind = serde_json::from_value(params["action_kind"].clone())
                        .map_err(|_| Failure::new("browser_invalid_params"))?;
                    let diagnostic = page.diagnose(reference, kind, deadline).await?;
                    let mut value =
                        serde_json::to_value(diagnostic).map_err(|_| sem::types::decode())?;
                    value["tab_id"] = port.tab_id()?.into();
                    Ok(value)
                }
                "browser_screenshot" => capture(&page.access(false)?, deadline).await.map(|v| v.0),
                "browser_act" => {
                    let attachment = if params["action"]["kind"] == "attach_file" {
                        if !self.settings.uploads {
                            return Err(Failure::new("browser_upload_disabled"));
                        }
                        Some(files::Attachment {
                            files: self.files.clone(),
                            owner: files::Owner {
                                principal: principal.clone(),
                                session: id.into(),
                                tab: tab.clone(),
                                origin: String::new(),
                                control: slot.control.clone(),
                            },
                            grant: params["action"]["file_grant_id"].as_str().unwrap().into(),
                            operation: operation_id
                                .ok_or_else(|| Failure::new("browser_invalid_params"))?
                                .into(),
                            epoch,
                        })
                    } else {
                        None
                    };
                    act(
                        &mut session,
                        &page,
                        &port,
                        params,
                        paused,
                        deadline,
                        attachment,
                    )
                    .await
                }
                "browser_wait" => {
                    session.invalidate();
                    loop {
                        let (_, meta) = page.access(false)?.metadata(deadline).await?;
                        if let Some(text) = params["url_contains"].as_str() {
                            if meta.url.contains(text) {
                                break Ok(json!({"condition_observed":true,"url":meta.url}));
                            }
                        } else if let Some(text) = params["text_contains"].as_str() {
                            let observation = page
                                .observe(
                                    sem::Query {
                                        find: Some(text.into()),
                                        ..Default::default()
                                    },
                                    200,
                                    20000,
                                    None,
                                    deadline,
                                )
                                .await?;
                            if observation.rows.iter().any(|r| {
                                r.facts.name.contains(text)
                                    || r.facts.value.as_ref().is_some_and(|v| v.contains(text))
                            }) {
                                break Ok(json!({"condition_observed":true}));
                            }
                        }
                        if Instant::now() + Duration::from_millis(100) >= deadline {
                            break Err(Failure::new("browser_condition_timeout"));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                _ => Err(Failure::new("browser_feature_unsupported")),
            }
        }
        .await;
        *slot.active.lock().unwrap() = session.active.clone();
        if result.as_ref().is_err_and(|e| e.state == "unknown") {
            slot.pause();
            session.invalidate();
        }
        let mut value = result?;
        if slot.control.snapshot() != (epoch, paused) {
            return Err(Failure::unknown("browser_control_changed"));
        }
        value["session_id"] = id.into();
        value["control_epoch"] = epoch.into();
        value["selection"] = serde_json::to_value(&slot.selection).unwrap();
        if operation == "browser_inspect" {
            sem::actionability::check_size(&value)?;
        }
        Ok(value)
    }
    async fn v2_control(
        &self,
        id: &str,
        slot: &Arc<Slot>,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        let action = params["action"].as_str().unwrap_or("");
        if action == "close" {
            slot.stop();
        } else if action == "pause" {
            slot.pause();
        }
        let mut session = if action == "close" {
            tokio::time::timeout_at(deadline.into(), slot.session.lock())
                .await
                .map_err(|_| Failure::unknown("browser_input_pending"))?
        } else {
            slot.session
                .try_lock()
                .map_err(|_| Failure::unknown("browser_input_pending"))?
        };
        session.invalidate();
        session.touched = Instant::now();
        if action == "close" {
            session.close().await?;
            self.v2_sessions.lock().unwrap().remove(id);
            return Ok(json!({"session_id":id,"state":"closed"}));
        }
        let page = sem::Page::new(slot.port()?);
        match action {
            "resume" => {
                if session.human.is_some() {
                    return Err(Failure::new("browser_paused"));
                }
                page.resume(
                    params["control_epoch"].as_u64().unwrap_or(0),
                    true,
                    deadline,
                )
                .await?;
            }
            "pause" => {
                page.settled(deadline).await?;
            }
            _ => return Err(Failure::new("browser_feature_unsupported")),
        }
        let (epoch, paused) = slot.control.snapshot();
        Ok(
            json!({"session_id":id,"control_epoch":epoch,"controller":if paused{"paused"}else{"ai"},"new_observation_required":true}),
        )
    }
    pub(super) fn v2_pause_all(&self, closing: bool) {
        for slot in self.v2_sessions.lock().unwrap().values() {
            if closing {
                slot.stop();
            } else {
                slot.pause();
            }
        }
    }
    pub(super) fn v2_expire_offline(self: &Arc<Self>) {
        let slots: Vec<_> = self
            .v2_sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        for (_, slot) in &slots {
            slot.stop();
        }
        let all = self.v2_sessions.clone();
        tokio::spawn(async move {
            for (id, slot) in slots {
                if slot.session.lock().await.close().await.is_ok() {
                    all.lock().unwrap().remove(&id);
                }
            }
        });
    }
    pub(super) async fn v2_close_all(&self) -> bool {
        let slots: Vec<_> = self
            .v2_sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        let mut stopped = true;
        for (id, slot) in slots {
            slot.stop();
            if slot.session.lock().await.close().await.is_ok() {
                self.v2_sessions.lock().unwrap().remove(&id);
            } else {
                stopped = false;
            }
        }
        stopped
    }
}
async fn observe(
    session: &mut Session,
    page: &sem::Page<native::Port>,
    port: &native::Port,
    params: &Value,
    deadline: Instant,
) -> Result<Value> {
    let continuation = if let Some(cursor) = params["cursor"].as_str() {
        let previous = session
            .cursor
            .take()
            .filter(|(id, _, _)| id == cursor)
            .ok_or_else(|| Failure::new("browser_stale_cursor"))?;
        Some((previous.1, previous.2))
    } else {
        None
    };
    let query = if let Some((_, query)) = &continuation {
        query.clone()
    } else {
        let within = params["within"]
            .as_str()
            .map(|id| {
                session
                    .frames
                    .get(id)
                    .copied()
                    .ok_or_else(|| Failure::new("browser_stale_reference"))
            })
            .transpose()?;
        sem::Query {
            filter: match params["filter"].as_str() {
                Some("interactive") => sem::Filter::Interactive,
                Some("viewport") => sem::Filter::Viewport,
                _ => sem::Filter::All,
            },
            find: params["find"].as_str().map(str::to_owned),
            within,
        }
    };
    let before = port.frames(deadline).await?;
    let result = page
        .observe(
            query.clone(),
            params["max_nodes"].as_u64().unwrap_or(200) as usize,
            params["max_chars"].as_u64().unwrap_or(20000) as usize,
            continuation.map(|v| v.0),
            deadline,
        )
        .await?;
    session.references.clear();
    session.reference_tab = None;
    session.cursor = None;
    session.frames.clear();
    let mut nodes = Vec::new();
    for row in result.rows {
        let frame = if let Some((id, _)) = session
            .frames
            .iter()
            .find(|(_, context)| **context == row.context)
        {
            id.clone()
        } else {
            let id = token()?;
            session.frames.insert(id.clone(), row.context);
            id
        };
        let facts = row.facts;
        let entries: Vec<_> = facts.record.entries.into_iter().map(|entry|json!({"tag":entry.tag,"attributes":entry.attributes,"text":entry.text,"value":entry.value,"checked":entry.checked})).collect();
        let mut node = json!({"role":facts.role,"kind":facts.kind,"tag":facts.tag,"input_type":facts.input_type,"name":facts.name,"name_source":facts.name_source,"name_quality":facts.name_quality,"frame_id":frame,
            "file_input":facts.tag=="input"&&facts.input_type=="file","enabled":facts.enabled,"readonly":facts.readonly,"editable":facts.editable,"sensitive":facts.sensitive,"checked":facts.checked,"in_view":facts.in_view,"record_complete":facts.record.complete,
            "record":{"entries":entries,"complete":facts.record.complete,"limits":facts.record.limits}});
        if let Some(value) = facts.value {
            node["value"] = value.into();
        }
        if let Some(reference) = row.reference {
            let id = token()?;
            session.references.insert(id.clone(), reference);
            node["ref"] = id.into();
        }
        nodes.push(node);
    }
    let (_, meta) = page.access(false)?.metadata(deadline).await?;
    if !same_frames(&before, &port.frames(deadline).await?) {
        session.invalidate();
        return Err(Failure::new("browser_stale_reference"));
    }
    session.reference_tab = Some(port.tab_id()?);
    let mut value = json!({"observation_profile":"semantic_dom_v1","nodes":nodes,"page":meta.public(),
        "coverage":{"complete":result.complete,"changed_during_read":result.changed_during_read,"omitted":result.omitted,"scanned":result.scanned},"content_trust":"untrusted_page"});
    value["page"]["tab_id"] = port.tab_id()?.into();
    if let Some(cursor) = result.cursor {
        let id = token()?;
        session.cursor = Some((id.clone(), cursor, query));
        value["next_cursor"] = id.into();
    }
    Ok(value)
}
async fn act(
    session: &mut Session,
    page: &sem::Page<native::Port>,
    port: &native::Port,
    params: &Value,
    paused: bool,
    deadline: Instant,
    attachment: Option<files::Attachment>,
) -> Result<Value> {
    let action = &params["action"];
    let kind = action["kind"].as_str().unwrap_or("");
    let reference = action["ref"]
        .as_str()
        .map(|id| session.reference(id, &port.tab_id()?))
        .transpose()?;
    session.invalidate();
    if let Some(reference) = reference {
        let input = match kind {
            "attach_file" => sem::Action::AttachFile(
                attachment.ok_or_else(|| Failure::new("browser_upload_denied"))?,
            ),
            "click" => sem::Action::Click,
            "check" => sem::Action::Check(action["checked"].as_bool().unwrap()),
            "fill" => sem::Action::ReplaceText(action["text"].as_str().unwrap().into()),
            "press" => sem::Action::Press(sem::Key::parse(action["key"].as_str().unwrap())?),
            "select" => sem::Action::SetFormValue(action["value"].as_str().unwrap().into()),
            _ => return Err(Failure::new("browser_feature_unsupported")),
        };
        let mut value = serde_json::to_value(page.act(reference, input, deadline).await?)
            .map_err(crate::browser::io_error)?;
        if kind == "attach_file" {
            value["file_selected"] = true.into();
            value["file_grant_id"] = action["file_grant_id"].clone();
        }
        return Ok(value);
    }
    match kind {
        "navigate" => {
            let url = action["url"].as_str().unwrap().to_owned();
            page.command(false, false, deadline, move |p| async move {
                p.navigate(&url, deadline).await
            })
            .await?;
            Ok(json!({"navigation_started":true,"business_result_verified":false}))
        }
        "dialog" => {
            let id = action["dialog_id"].as_str().unwrap().to_owned();
            let accept = action["accept"].as_bool().unwrap();
            page.command(paused, true, deadline, move |p| async move {
                p.handle_dialog(&id, accept, deadline).await
            })
            .await?;
            Ok(json!({"input_acknowledged":true,"business_result_verified":false}))
        }
        "new_tab" => {
            let id = page
                .command(false, false, deadline, move |p| async move {
                    p.new_tab(deadline).await
                })
                .await?;
            session.active = id.clone();
            Ok(json!({"tab_id":id}))
        }
        "close_tab" => {
            page.command(false, false, deadline, move |p| async move {
                p.close_tab(deadline).await
            })
            .await?;
            session.active = port
                .tabs()
                .first()
                .ok_or_else(|| Failure::new("browser_tab_unavailable"))?
                .tab_id
                .clone();
            Ok(json!({"tab_id":session.active}))
        }
        "select_tab" => {
            page.command(false, false, deadline, move |p| async move {
                p.activate(deadline).await
            })
            .await?;
            session.active = port.tab_id()?;
            Ok(json!({"tab_id":session.active}))
        }
        "scroll" => {
            let delta = action["delta_y"].as_f64().unwrap();
            page.command(false, false, deadline, move |p| async move {
                let frames = p.frames(deadline).await?;
                let frame = frames.first().ok_or_else(sem::types::stale)?;
                match p
                    .program(
                        frame,
                        sem::program::Program::ScrollViewport { delta },
                        deadline,
                    )
                    .await?
                {
                    sem::program::Output::Applied(Some(true)) => Ok(()),
                    _ => Err(Failure::new("browser_invalid_params")),
                }
            })
            .await?;
            let (_, m) = page.access(false)?.metadata(deadline).await?;
            Ok(
                json!({"scroll_y":m.scroll_y,"input_acknowledged":true,"business_result_verified":false}),
            )
        }
        _ => Err(Failure::new("browser_feature_unsupported")),
    }
}
