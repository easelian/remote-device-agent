use super::{
    Failure, Principal, Result, Settings,
    cdp::{Cdp, Control},
    files::{self, Files},
    page::{self, Element},
    process::{self, BrowserProcess},
    profiles::{self, Profiles},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;
mod human;
use human::HumanLease;

const MAX_SESSIONS: usize = 2;
const MAX_TABS: usize = 8;
const IDLE: Duration = Duration::from_secs(600);

#[derive(Clone)]
struct Target {
    id: String,
    session: String,
    kind: String,
    url: String,
    title: String,
    dialog: Option<Value>,
    parent: Option<String>,
    ready: bool,
}
struct Slot {
    owner: String,
    user: String,
    profile_name: Option<String>,
    control: Arc<Control>,
    closing: AtomicBool,
    transport: Mutex<Option<Cdp>>,
    session: AsyncMutex<Session>,
}
struct Session {
    process: Option<BrowserProcess>,
    targets: Arc<Mutex<HashMap<String, Target>>>,
    references: HashMap<String, Element>,
    active: String,
    observed_tab: String,
    touched: Instant,
    human: Option<HumanLease>,
    guard: Option<tokio::task::JoinHandle<()>>,
}

#[tokio::test]
async fn cancelled_close_retains_event_task_ownership() {
    let done = Arc::new(tokio::sync::Notify::new());
    let signal = done.clone();
    let mut session = Session {
        process: None,
        targets: Arc::new(Mutex::new(HashMap::new())),
        references: HashMap::new(),
        active: String::new(),
        observed_tab: String::new(),
        touched: Instant::now(),
        human: None,
        guard: Some(tokio::spawn(async move {
            signal.notified().await;
        })),
    };
    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_millis(5), session.close())
                .await
                .is_err()
        );
        assert!(session.guard.is_some());
    }
    done.notify_one();
    tokio::time::timeout(Duration::from_secs(1), session.close())
        .await
        .unwrap()
        .unwrap();
    assert!(session.guard.is_none());
}

impl Session {
    async fn close(&mut self) -> Result<()> {
        if let Some(process) = self.process.as_mut() {
            process.close().await?;
        }
        if let Some(guard) = self.guard.as_mut() {
            let result = guard.await;
            self.guard = None;
            result.map_err(|_| Failure::unknown("browser_cleanup_pending"))?;
        }
        self.process = None;
        Ok(())
    }
}
struct Admission {
    online: bool,
    closing: bool,
}
pub struct Browsers {
    settings: Settings,
    executable: Option<PathBuf>,
    root: Option<PathBuf>,
    sessions: Arc<Mutex<HashMap<String, Arc<Slot>>>>,
    initialized: Mutex<bool>,
    admission: Mutex<Admission>,
    profiles: Profiles,
    root_lock: Mutex<Option<Arc<crate::profile_lock::ProfileLocks>>>,
    files: Arc<Files>,
}

impl Browsers {
    pub fn new(settings: &Settings, config_path: Option<&Path>, token: &str) -> Self {
        let root = config_path.and_then(Path::parent).map(|p| {
            p.join(".rdh-browser")
                .join(&crate::config::sha256_hex(token.as_bytes())[..24])
        });
        let files = Files::new(root.as_deref());
        Self {
            settings: settings.clone(),
            executable: process::detect(settings.executable.as_deref()),
            root,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            initialized: Mutex::new(false),
            admission: Mutex::new(Admission {
                online: true,
                closing: false,
            }),
            profiles: Profiles::default(),
            root_lock: Mutex::new(None),
            files,
        }
    }
    pub fn info(&self) -> Value {
        let reason = if !self.settings.enabled {
            Some("disabled_by_local_policy")
        } else if self.root.is_none() {
            Some("private_storage_unavailable")
        } else if self.executable.is_none() {
            Some("browser_not_installed")
        } else {
            None
        };
        json!({"version":1,"available":reason.is_none(),"reason":reason,"mode":"managed_headless","profiles":if self.settings.persistent_profiles {vec!["temporary","named"]}else{vec!["temporary"]},"max_sessions":MAX_SESSIONS,"max_tabs":MAX_TABS,"idle_seconds":IDLE.as_secs(),"principal_version":1,"network_isolation":false,"human_takeover":true,"screenshot":true,"file_transfer":self.settings.downloads||self.settings.uploads,"downloads":{"enabled":self.settings.downloads,"http_attachment_only":true,"declared_length_required":true,"max_file_bytes":files::MAX_FILE,"max_total_bytes":files::MAX_TOTAL,"retention_seconds":files::RETENTION},"uploads":{"enabled":self.settings.uploads,"source":"user_local_file","main_frame_only":true,"max_file_bytes":files::MAX_FILE,"grant_ttl_seconds":files::UPLOAD_TTL,"uses":1}})
    }
    fn prepare(&self) -> Result<&Path> {
        if self.info()["available"] != true {
            return Err(Failure::new("browser_unavailable"));
        }
        let root = self.root.as_deref().unwrap();
        let mut initialized = self.initialized.lock().unwrap();
        if !*initialized {
            let parent = root.parent().unwrap();
            for path in [parent, root] {
                match crate::storage::create_private_dir(path) {
                    Ok(_) => {}
                    Err(crate::error::Error::Io(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        crate::storage::private_metadata(path, true).map_err(super::io_error)?;
                    }
                    Err(error) => return Err(super::io_error(error)),
                }
            }
            let lock_path = root.join("agent");
            crate::profile_lock::ProfileLocks::recover(std::slice::from_ref(&lock_path))
                .map_err(|_| Failure::new("browser_profile_busy"))?;
            let lock = crate::profile_lock::ProfileLocks::acquire(&[lock_path])
                .map_err(|_| Failure::new("browser_profile_busy"))?;
            process::recover(root)?;
            self.files.prepare()?;
            let lock = Arc::new(lock);
            *self.root_lock.lock().unwrap() = Some(lock.clone());
            self.files.start(lock);
            *initialized = true;
        }
        Ok(root)
    }
    pub fn dispatch(
        &self,
        operation: &str,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
        operation_id: Option<&str>,
    ) -> Result<Value> {
        if !principal.valid() {
            return Err(Failure::new("browser_unauthorized"));
        }
        if (operation == "browser_human") != (principal.channel == "human") {
            return Err(Failure::new("browser_unauthorized"));
        }
        if operation == "browser_human" {
            return tokio::runtime::Handle::current()
                .block_on(self.human(params, principal, deadline));
        }
        validate(operation, params)?;
        tokio::runtime::Handle::current().block_on(self.execute(
            operation,
            params,
            principal,
            deadline,
            operation_id,
        ))
    }
    async fn execute(
        &self,
        operation: &str,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
        operation_id: Option<&str>,
    ) -> Result<Value> {
        self.prepare()?;
        self.expire().await;
        if operation == "browser_open" {
            return self.open(params, principal, deadline).await;
        }
        if operation == "browser_profiles" {
            return self.profiles.list(self.root.as_deref().unwrap(), principal);
        }
        if operation == "browser_files" {
            return self.files.list(principal);
        }
        if operation == "browser_control" && params["action"] == "delete_file" {
            return self
                .files
                .delete(principal, params["artifact_id"].as_str().unwrap());
        }
        if operation == "browser_control" && params["action"] == "delete_profile" {
            return self.profiles.delete(
                self.root.as_deref().unwrap(),
                principal,
                params["profile_name"].as_str().unwrap(),
            );
        }
        if operation == "browser_sessions" {
            let slots: Vec<_> = self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, s)| s.owner == principal.owner)
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect();
            let mut sessions = Vec::new();
            for (id, slot) in slots {
                let (epoch, paused) = slot.control.snapshot();
                let state = match slot.session.try_lock() {
                    Ok(s) if s.process.as_ref().is_some_and(|p| p.cdp.connected()) => "ready",
                    Ok(s) if s.process.is_none() => "closed",
                    Ok(_) => "lost",
                    Err(_) => "busy",
                };
                sessions.push(json!({"session_id":id,"state":state,"control_epoch":epoch,"controller":if paused {"paused"} else {"ai"},"profile_name":slot.profile_name}));
            }
            return Ok(json!({"sessions":sessions}));
        }
        let id = params["session_id"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let slot = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.owner == principal.owner)
            .cloned()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        if operation == "browser_control" {
            return self.control(id, &slot, params, deadline).await;
        }
        let (epoch, paused) = slot.control.snapshot();
        let resolving_dialog = operation == "browser_act" && params["action"]["kind"] == "dialog";
        if (paused && operation != "browser_tabs" && !resolving_dialog)
            || slot.closing.load(Ordering::Acquire)
        {
            return Err(Failure::new("browser_paused"));
        }
        if operation != "browser_tabs" && params["control_epoch"].as_u64() != Some(epoch) {
            return Err(Failure::new("browser_control_changed"));
        }
        let mut session = slot
            .session
            .try_lock()
            .map_err(|_| Failure::new("browser_busy"))?;
        if session.human.is_some() {
            return Err(Failure::new("browser_paused"));
        }
        session.touched = Instant::now();
        let process = session
            .process
            .as_ref()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        if !process.cdp.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        if process.cdp.pending() != 0 && operation != "browser_tabs" && !resolving_dialog {
            return Err(Failure::unknown("browser_input_pending"));
        }
        let cdp = if paused {
            process.cdp.human(slot.control.clone(), epoch)
        } else {
            process.cdp.authorized(slot.control.clone(), epoch)
        };
        let (cdp, effects) = cdp.track_effects();
        if operation == "browser_tabs" {
            let tabs: Vec<_> = session
                .targets
                .lock()
                .unwrap()
                .values()
                .filter(|t| t.kind == "page")
                .map(|t| json!({"tab_id":t.id,"url":t.url,"title":t.title,"dialog":t.dialog,"ready":t.ready}))
                .collect();
            return Ok(
                json!({"session_id":id,"control_epoch":epoch,"active_tab":session.active,"tabs":tabs,"controller":if paused{"paused"}else{"ai"},"input_pending":cdp.pending()!=0}),
            );
        }
        let tab = params
            .get("tab_id")
            .and_then(Value::as_str)
            .unwrap_or(&session.active)
            .to_owned();
        let target = session
            .targets
            .lock()
            .unwrap()
            .get(&tab)
            .filter(|t| t.kind == "page")
            .cloned()
            .ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
        if !(target.ready || operation == "browser_act" && params["action"]["kind"] == "close_tab")
        {
            return Err(Failure::new("browser_busy"));
        }
        let outcome = async { match operation {
            "browser_screenshot" => {
                if target.dialog.is_some() {
                    return Err(Failure::new("browser_dialog_open"));
                }
                let before = page::frame_tree(&cdp, &target.session, deadline).await?;
                let mut value = super::image::capture(&cdp, &target.session, deadline).await?;
                let after = page::frame_tree(&cdp, &target.session, deadline).await?;
                if before.first().map(|f| (&f.id, &f.loader)) != after.first().map(|f| (&f.id, &f.loader)) {
                    return Err(Failure::new("browser_stale_frame"));
                }
                value["page"] = page::metadata(&cdp,after.first().ok_or_else(||Failure::new("browser_tab_unavailable"))?,deadline).await?;
                value["page"]["tab_id"] = tab.clone().into();
                Ok(value)
            }
            "browser_observe" => {
                if target.dialog.is_some() {
                    return Err(Failure::new("browser_dialog_open"));
                }
                let frames = frames(&cdp, &session, &target, deadline).await?;
                let (mut value, refs) = page::observe(
                    &cdp,
                    &frames,
                    params["max_nodes"].as_u64().unwrap_or(200) as usize,
                    params["max_chars"].as_u64().unwrap_or(20000) as usize,
                    deadline,
                )
                .await?;
                session.references = refs;
                session.observed_tab = tab.clone();
                value["page"] = page::metadata(&cdp,frames.first().ok_or_else(||Failure::new("browser_tab_unavailable"))?,deadline).await?;
                value["page"]["tab_id"] = tab.clone().into();
                Ok(value)
            }
            "browser_act" => {
                let action = &params["action"];
                match action["kind"].as_str().unwrap_or("") {
                    "scroll" => {
                        if target.dialog.is_some() { return Err(Failure::new("browser_dialog_open")); }
                        let tree = page::frame_tree(&cdp, &target.session, deadline).await?;
                        let frame = tree.first().ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
                        session.references.clear();
                        page::scroll(&cdp, frame, &action["delta_y"], deadline).await
                    }
                    "navigate" => {
                        session.references.clear();
                        let reply = cdp
                            .call(
                                Some(&target.session),
                                "Page.navigate",
                                json!({"url":action["url"]}),
                                deadline,
                            )
                            .await?;
                        if reply.get("errorText").is_some() {
                            Err(Failure::unknown("browser_navigation_failed"))
                        } else {
                            Ok(json!({"navigation_started":true,"business_result_verified":false}))
                        }
                    }
                    "new_tab" => {
                        if session
                            .targets
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|t| t.kind == "page")
                            .count()
                            >= MAX_TABS
                        {
                            return Err(Failure::new("browser_tab_limit"));
                        }
                        let result = cdp
                            .call(
                                None,
                                "Target.createTarget",
                                json!({"url":"about:blank"}),
                                deadline,
                            )
                            .await?;
                        let new_tab = result["targetId"]
                            .as_str()
                            .ok_or_else(|| Failure::unknown("browser_command_failed"))?
                            .to_owned();
                        wait_target(&cdp, &session.targets, &new_tab, deadline).await?;
                        session.active = new_tab.clone();
                        Ok(json!({"tab_id":new_tab}))
                    }
                    "close_tab" => {
                        if session
                            .targets
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|t| t.kind == "page")
                            .count()
                            <= 1
                        {
                            return Err(Failure::new("browser_last_tab"));
                        }
                        cdp.call(
                            None,
                            "Target.closeTarget",
                            json!({"targetId":tab}),
                            deadline,
                        )
                        .await?;
                        session.targets.lock().unwrap().remove(&tab);
                        if session.active == tab {
                            let next_active = session
                                .targets
                                .lock()
                                .unwrap()
                                .values()
                                .find(|t| t.kind == "page" && t.ready)
                                .map(|t| t.id.clone())
                                .unwrap_or_default();
                            session.active = next_active;
                        }
                        session.references.clear();
                        Ok(json!({"tab_closed":true}))
                    }
                    "select_tab" => {
                        cdp.call(Some(&target.session),"Page.bringToFront",json!({}),deadline).await?;
                        session.active = tab;
                        session.references.clear();
                        Ok(json!({"active_tab":session.active}))
                    }
                    "dialog" => {
                        handle_dialog(&cdp,&session.targets,&target,&action["dialog_id"],&action["accept"],deadline).await?;
                        Ok(json!({"dialog_handled":true,"controller":if paused{"paused"}else{"ai"},"input_pending":cdp.pending()!=0}))
                    }
                    _ => {
                        if target.dialog.is_some() {
                            return Err(Failure::new("browser_dialog_open"));
                        }
                        if session.observed_tab != tab { return Err(Failure::new("browser_stale_reference")); }
                        let reference = action["ref"].as_str().unwrap_or("");
                        let element = session
                            .references
                            .get(reference)
                            .ok_or_else(|| Failure::new("browser_stale_reference"))?;
                        let result = if action["kind"] == "attach_file" {
                            if !self.settings.uploads { return Err(Failure::new("browser_upload_disabled")); }
                            let operation_id = operation_id.ok_or_else(||Failure::new("browser_unauthorized"))?;
                            let tree = page::frame_tree(&cdp, &target.session, deadline).await?;
                            let top = tree.first().ok_or_else(||Failure::new("browser_tab_unavailable"))?;
                            let origin = element.prepare_file(&cdp, top, deadline).await?;
                            let path = self.files.consume_upload(&files::Owner{principal:principal.clone(),session:id.into(),tab:tab.clone(),origin,control:slot.control.clone()},action["file_grant_id"].as_str().unwrap_or(""),operation_id)?;
                            element.attach_file(&cdp, &path, deadline).await.map(|mut value|{value["file_grant_id"]=action["file_grant_id"].clone();value})
                        } else { element.act(&cdp, action, deadline).await };
                        session.references.clear();
                        result
                    }
                }
            }
            "browser_wait" => {
                let expected = params["url_contains"].as_str().unwrap_or("");
                let text = params["text_contains"].as_str();
                loop {
                    let tree = frames(&cdp, &session, &target, deadline).await?;
                    if let Some(text) = text {
                        let (value, _) = page::observe(&cdp, &tree, 200, 20000, deadline).await?;
                        if value["nodes"].as_array().is_some_and(|v|v.iter().any(|n|n["name"].as_str().is_some_and(|s|s.contains(text)))) {
                            break Ok(json!({"condition_observed":true,"observation_id":value["observation_id"],"coverage":value["coverage"]}));
                        }
                    } else if tree.first().is_some_and(|frame| frame.url.contains(expected)) {
                        break Ok(json!({"condition_observed":true,"url":tree[0].url}));
                    }
                    if Instant::now() + Duration::from_millis(100) >= deadline {
                        break Err(Failure::new("browser_condition_timeout"));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            _ => Err(Failure::new("browser_invalid_params")),
        }
        }.await;
        let outcome = outcome.map_err(|mut error| {
            if operation == "browser_act" && effects.load(Ordering::Acquire) {
                error.state = "unknown";
            }
            error
        });
        if outcome.as_ref().is_err_and(|e| e.state == "unknown") {
            slot.control.change(true);
            session.references.clear();
        }
        let mut value = outcome?;
        // A concurrent pause never leaks a completed observation or screenshot from the old epoch.
        if slot.control.snapshot() != (epoch, paused) {
            return Err(Failure::unknown("browser_control_changed"));
        }
        value["session_id"] = id.into();
        value["control_epoch"] = epoch.into();
        Ok(value)
    }

    async fn open(
        &self,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
    ) -> Result<Value> {
        let profile_name = params["profile_name"].as_str();
        if profile_name.is_some() && !self.settings.persistent_profiles {
            return Err(Failure::new("browser_profile_disabled"));
        }
        let id = crate::random::hex_token::<16>().map_err(super::io_error)?;
        let slot = Arc::new(Slot {
            owner: principal.owner.clone(),
            user: principal.user.clone(),
            profile_name: profile_name.map(str::to_owned),
            control: Arc::new(Control::new()),
            closing: AtomicBool::new(false),
            transport: Mutex::new(None),
            session: AsyncMutex::new(Session {
                process: None,
                targets: Arc::new(Mutex::new(HashMap::new())),
                references: HashMap::new(),
                active: String::new(),
                observed_tab: String::new(),
                touched: Instant::now(),
                human: None,
                guard: None,
            }),
        });
        {
            let admission = self.admission.lock().unwrap();
            if !admission.online || admission.closing {
                return Err(Failure::new("browser_paused"));
            }
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.len() >= MAX_SESSIONS {
                return Err(Failure::new("browser_session_limit"));
            }
            sessions.insert(id.clone(), slot.clone());
        }
        let mut session = slot.session.lock().await;
        let result=async {
            let named = profile_name.map(|name|self.profiles.open(self.root.as_deref().unwrap(),principal,name)).transpose()?;
            let process=BrowserProcess::launch(self.executable.as_deref().unwrap(),self.root.as_deref().unwrap(),&id,named,deadline).await?;
            let cdp=process.cdp.authorized(slot.control.clone(), 1);
            *slot.transport.lock().unwrap()=Some(cdp.clone());
            session.process=Some(process);
            if slot.closing.load(Ordering::Acquire) || slot.control.snapshot() != (1, false) {
                cdp.stop();
                return Err(Failure::new("browser_control_changed"));
            }
            session.guard=Some(guard(cdp.cleanup(),session.targets.clone(),self.settings.downloads.then(||DownloadContext{files:self.files.clone(),principal:principal.clone(),session:id.clone(),control:slot.control.clone()})));
            cdp.call(None,"Browser.setDownloadBehavior",json!({"behavior":"deny","eventsEnabled":true}),deadline).await?;
            cdp.call(None,"Target.setDiscoverTargets",json!({"discover":true}),deadline).await?;
            cdp.call(None,"Target.setAutoAttach",json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),deadline).await?;
            let target=loop {
                if let Some(target)=session.targets.lock().unwrap().values().find(|t|t.kind=="page"&&t.ready).cloned(){break target;}
                if !cdp.connected() || slot.closing.load(Ordering::Acquire) || slot.control.snapshot() != (1, false) { return Err(Failure::unknown("browser_control_changed")); }
                if Instant::now()>=deadline {return Err(Failure::unknown("browser_timeout_unknown"));}
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            session.active=target.id.clone();
            if let Some(url)=params["url"].as_str(){
                cdp.call(Some(&target.session),"Page.navigate",json!({"url":url}),deadline).await?;
            }
            if slot.control.snapshot() != (1, false) { return Err(Failure::unknown("browser_control_changed")); }
            Ok(json!({"session_id":id,"tab_id":target.id,"control_epoch":1,"state":"ready","profile":if profile_name.is_some(){"named"}else{"temporary"},"profile_name":profile_name,"profile_id":profile_name.map(|name|profiles::id(principal,name))}))
        }.await;
        if result.is_err() {
            let _ = session.close().await;
            self.sessions.lock().unwrap().remove(&id);
        }
        if result.is_ok() {
            let slot = Arc::downgrade(&slot);
            let sessions = Arc::downgrade(&self.sessions);
            let id = id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let (Some(slot), Some(sessions)) = (slot.upgrade(), sessions.upgrade()) else {
                        break;
                    };
                    if let Ok(mut session) = slot.session.try_lock() {
                        if session.process.is_none() {
                            break;
                        }
                        if session.touched.elapsed() >= IDLE {
                            slot.control.change(true);
                            if session.close().await.is_err() {
                                continue;
                            }
                            sessions.lock().unwrap().remove(&id);
                            break;
                        }
                    }
                }
            });
        }
        result
    }

    async fn control(
        &self,
        id: &str,
        slot: &Arc<Slot>,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        let action = params["action"].as_str().unwrap_or("");
        if action == "pause" || action == "close" {
            slot.control.change(true);
        }
        if action == "close" {
            slot.closing.store(true, Ordering::Release);
            if let Some(cdp) = slot.transport.lock().unwrap().as_ref() {
                cdp.stop();
            }
        }
        let mut session = if action == "close" {
            tokio::time::timeout_at(deadline.into(), slot.session.lock())
                .await
                .map_err(|_| Failure::unknown("browser_input_pending"))?
        } else {
            // Fencing is immediate; do not occupy the emergency-control slot while
            // waiting for a stalled action. The caller can inspect or close now.
            slot.session
                .try_lock()
                .map_err(|_| Failure::unknown("browser_input_pending"))?
        };
        session.references.clear();
        session.touched = Instant::now();
        if action == "resume" && session.human.is_some() {
            return Err(Failure::new("browser_paused"));
        }
        let process = session
            .process
            .as_mut()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        match action {
            "close" => {
                session.close().await?;
                self.sessions.lock().unwrap().remove(id);
                Ok(json!({"session_id":id,"state":"closed"}))
            }
            "pause" | "resume" => {
                if !process.cdp.connected() {
                    return Err(Failure::unknown("browser_lost"));
                }
                if process.cdp.pending() != 0 {
                    return Err(Failure::unknown("browser_input_pending"));
                }
                if action == "resume" {
                    slot.control
                        .resume(params["control_epoch"].as_u64().unwrap_or(0))?;
                }
                let (epoch, paused) = slot.control.snapshot();
                Ok(
                    json!({"session_id":id,"control_epoch":epoch,"controller":if paused{"paused"}else{"ai"},"new_observation_required":true}),
                )
            }
            _ => Err(Failure::new("browser_invalid_params")),
        }
    }
    async fn expire(&self) {
        let slots: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect();
        for (id, slot) in slots {
            if let Ok(mut session) = slot.session.try_lock()
                && session.touched.elapsed() >= IDLE
            {
                slot.control.change(true);
                if session.close().await.is_err() {
                    continue;
                }
                self.sessions.lock().unwrap().remove(&id);
            }
        }
    }
    pub fn stop_all(&self) {
        self.files.stop();
        let mut admission = self.admission.lock().unwrap();
        admission.closing = true;
        admission.online = false;
        for slot in self.sessions.lock().unwrap().values() {
            slot.control.change(true);
            slot.closing.store(true, Ordering::Release);
            if let Some(cdp) = slot.transport.lock().unwrap().as_ref() {
                cdp.stop();
            }
        }
    }
    pub fn pause_all(&self) {
        let mut admission = self.admission.lock().unwrap();
        admission.online = false;
        for slot in self.sessions.lock().unwrap().values() {
            slot.control.change(true);
        }
    }
    pub fn mark_online(&self) {
        let mut admission = self.admission.lock().unwrap();
        if !admission.closing {
            admission.online = true;
        }
    }
    pub fn expire_offline(self: &Arc<Self>) {
        let admission = self.admission.lock().unwrap();
        if admission.online {
            return;
        }
        let slots: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect();
        for (_, slot) in &slots {
            slot.control.change(true);
            slot.closing.store(true, Ordering::Release);
            if let Some(cdp) = slot.transport.lock().unwrap().as_ref() {
                cdp.stop();
            }
        }
        let browsers = Arc::clone(self);
        tokio::spawn(async move {
            for (id, slot) in slots {
                let mut session = slot.session.lock().await;
                if session.close().await.is_err() {
                    continue;
                }
                browsers.sessions.lock().unwrap().remove(&id);
            }
        });
    }
    pub async fn close(&self) -> bool {
        self.stop_all();
        let slots: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        let mut stopped = true;
        for slot in slots {
            let mut session = slot.session.lock().await;
            stopped &= session.close().await.is_ok();
        }
        if stopped {
            self.sessions.lock().unwrap().clear();
        }
        self.files.wait().await;
        stopped
    }
}

async fn wait_target(
    cdp: &Cdp,
    targets: &Arc<Mutex<HashMap<String, Target>>>,
    id: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        if !cdp.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        if targets.lock().unwrap().get(id).is_some_and(|t| t.ready) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Failure::unknown("browser_timeout_unknown"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
async fn frames(
    cdp: &Cdp,
    session: &Session,
    target: &Target,
    deadline: Instant,
) -> Result<Vec<page::Frame>> {
    let mut frames = page::frame_tree(cdp, &target.session, deadline).await?;
    let children: Vec<_> = session
        .targets
        .lock()
        .unwrap()
        .values()
        .filter(|t| t.kind == "iframe")
        .cloned()
        .collect();
    let mut selected = HashMap::from([(target.session.clone(), Vec::<(String, String)>::new())]);
    let mut remaining = children;
    loop {
        let Some(index) = remaining
            .iter()
            .position(|t| t.parent.as_ref().is_some_and(|p| selected.contains_key(p)))
        else {
            break;
        };
        let child = remaining.remove(index);
        let parent = child.parent.as_ref().unwrap();
        let mut ancestors = vec![(parent.clone(), child.id.clone())];
        ancestors.extend(selected.get(parent).unwrap().iter().cloned());
        selected.insert(child.session.clone(), ancestors.clone());
        for mut frame in page::frame_tree(cdp, &child.session, deadline).await? {
            frame.ancestors = ancestors.clone();
            if let Some(old) = frames.iter_mut().find(|f| f.id == frame.id) {
                *old = frame;
            } else {
                frames.push(frame);
            }
        }
    }
    Ok(frames)
}

struct DownloadContext {
    files: Arc<Files>,
    principal: Principal,
    session: String,
    control: Arc<Control>,
}
fn guard(
    cdp: Cdp,
    targets: Arc<Mutex<HashMap<String, Target>>>,
    downloads: Option<DownloadContext>,
) -> tokio::task::JoinHandle<()> {
    let mut events = cdp.events();
    tokio::spawn(async move {
        let mut initializers = tokio::task::JoinSet::new();
        loop {
            if !cdp.connected() {
                break;
            }
            let event = tokio::select! {
                joined=initializers.join_next(), if !initializers.is_empty()=>{
                    if joined.is_some_and(|r|r.is_err()){cdp.stop();break;}
                    continue;
                },
                event=events.recv()=>match event {Ok(event)=>event,Err(_)=>{cdp.stop();break;}},
                _=tokio::time::sleep(Duration::from_millis(500))=>{if !cdp.connected(){break;}else{continue;}},
            };
            if handle_event(&cdp, &targets, downloads.as_ref(), &mut initializers, event)
                .await
                .is_err()
            {
                cdp.stop();
                break;
            }
        }
        initializers.abort_all();
        while initializers.join_next().await.is_some() {}
    })
}
async fn initialize_target(cdp: &Cdp, session: &str, downloads: bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(125);
    let mut patterns =
        vec![json!({"urlPattern":"*","resourceType":"Document","requestStage":"Request"})];
    if downloads {
        patterns
            .push(json!({"urlPattern":"*","resourceType":"Document","requestStage":"Response"}));
    }
    cdp.call(
        Some(session),
        "Fetch.enable",
        json!({"patterns":patterns}),
        deadline,
    )
    .await?;
    cdp.call(
        Some(session),
        "Target.setAutoAttach",
        json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),
        deadline,
    )
    .await?;
    cdp.call(
        Some(session),
        "Runtime.runIfWaitingForDebugger",
        json!({}),
        deadline,
    )
    .await?;
    cdp.call(Some(session),"Network.setBlockedURLs",json!({"urls":["file:*","filesystem:*","javascript:*","chrome:*","chrome-extension:*","devtools:*","mailto:*","tel:*"]}),deadline).await?;
    if downloads {
        cdp.call(
            Some(session),
            "Network.setCacheDisabled",
            json!({"cacheDisabled":true}),
            deadline,
        )
        .await?;
    }
    cdp.call(
        Some(session),
        "Page.setInterceptFileChooserDialog",
        json!({"enabled":true}),
        deadline,
    )
    .await?;
    for method in [
        "Page.enable",
        "Runtime.enable",
        "Network.enable",
        "Accessibility.enable",
    ] {
        cdp.call(Some(session), method, json!({}), deadline).await?;
    }
    Ok(())
}
async fn handle_event(
    cdp: &Cdp,
    targets: &Arc<Mutex<HashMap<String, Target>>>,
    downloads: Option<&DownloadContext>,
    initializers: &mut tokio::task::JoinSet<()>,
    event: Value,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let params = &event["params"];
    match event["method"].as_str().unwrap_or("") {
        "Target.attachedToTarget" => {
            let info = &params["targetInfo"];
            let session = params["sessionId"]
                .as_str()
                .ok_or_else(|| Failure::unknown("browser_lost"))?;
            let id = info["targetId"].as_str().unwrap_or("");
            let kind = info["type"].as_str().unwrap_or("");
            if !matches!(kind, "page" | "iframe") {
                cdp.call(
                    Some(session),
                    "Runtime.runIfWaitingForDebugger",
                    json!({}),
                    deadline,
                )
                .await?;
                return Ok(());
            }
            if (kind == "page"
                && targets
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|t| t.kind == "page")
                    .count()
                    >= MAX_TABS)
                || targets.lock().unwrap().len() >= 40
                || initializers.len() >= 40
            {
                cdp.call(None, "Target.closeTarget", json!({"targetId":id}), deadline)
                    .await?;
                return Ok(());
            }
            targets.lock().unwrap().insert(
                id.into(),
                Target {
                    id: id.into(),
                    session: session.into(),
                    kind: kind.into(),
                    url: info["url"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(4096)
                        .collect(),
                    title: info["title"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(512)
                        .collect(),
                    dialog: None,
                    parent: event["sessionId"].as_str().map(str::to_owned),
                    ready: false,
                },
            );
            let (cdp, targets, id, session) = (
                cdp.clone(),
                targets.clone(),
                id.to_owned(),
                session.to_owned(),
            );
            let downloads = downloads.is_some();
            initializers.spawn(async move {
                // A popup can have no renderer document until its attachment
                // finishes. The event pump must keep serving Fetch meanwhile.
                let result = initialize_target(&cdp, &session, downloads).await;
                if result.is_ok() {
                    if let Some(target) = targets
                        .lock()
                        .unwrap()
                        .get_mut(&id)
                        .filter(|t| t.session == session)
                    {
                        target.ready = true;
                    }
                } else {
                    let present = targets
                        .lock()
                        .unwrap()
                        .get(&id)
                        .is_some_and(|t| t.session == session);
                    if present {
                        if cdp
                            .call(
                                None,
                                "Target.closeTarget",
                                json!({"targetId":id}),
                                Instant::now() + Duration::from_secs(1),
                            )
                            .await
                            .is_err()
                        {
                            cdp.stop();
                        }
                        targets.lock().unwrap().remove(&id);
                    }
                }
            });
        }
        "Target.detachedFromTarget" => {
            targets
                .lock()
                .unwrap()
                .retain(|_, t| t.session != params["sessionId"].as_str().unwrap_or(""));
        }
        "Target.targetInfoChanged" => {
            let info = &params["targetInfo"];
            if let Some(target) = targets
                .lock()
                .unwrap()
                .get_mut(info["targetId"].as_str().unwrap_or(""))
            {
                target.url = info["url"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(4096)
                    .collect();
                target.title = info["title"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(512)
                    .collect();
            }
        }
        "Fetch.requestPaused" => {
            let url = params["request"]["url"].as_str().unwrap_or("");
            let allowed = page::url_allowed(url) || url == "about:blank";
            if let Some(downloads) = downloads
                && allowed
                && params["responseHeaders"].as_array().is_some_and(|headers| {
                    headers.iter().any(|h| {
                        h["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case("content-disposition"))
                            && h["value"].as_str().is_some_and(|v| {
                                v.split(';')
                                    .next()
                                    .unwrap_or("")
                                    .trim()
                                    .eq_ignore_ascii_case("attachment")
                            })
                    })
                })
            {
                let target = targets
                    .lock()
                    .unwrap()
                    .values()
                    .find(|t| Some(t.session.as_str()) == event["sessionId"].as_str())
                    .cloned()
                    .ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
                let origin = url::Url::parse(url)
                    .map_err(|_| Failure::new("browser_url_denied"))?
                    .origin()
                    .ascii_serialization();
                let owner = files::Owner {
                    principal: downloads.principal.clone(),
                    session: downloads.session.clone(),
                    tab: target.id,
                    origin,
                    control: downloads.control.clone(),
                };
                if downloads
                    .files
                    .intercept(cdp.cleanup(), target.session, params, owner)
                    .is_err()
                {
                    cdp.call(
                        event["sessionId"].as_str(),
                        "Fetch.failRequest",
                        json!({"requestId":params["requestId"],"errorReason":"Aborted"}),
                        deadline,
                    )
                    .await?;
                }
                return Ok(());
            }
            let method = if allowed {
                "Fetch.continueRequest"
            } else {
                "Fetch.failRequest"
            };
            let mut args = json!({"requestId":params["requestId"]});
            if !allowed {
                args["errorReason"] = "BlockedByClient".into();
            }
            cdp.call(event["sessionId"].as_str(), method, args, deadline)
                .await?;
        }
        "Page.javascriptDialogOpening" => {
            let id = crate::random::hex_token::<16>().map_err(super::io_error)?;
            if let Some(target) = targets
                .lock()
                .unwrap()
                .values_mut()
                .find(|t| Some(t.session.as_str()) == event["sessionId"].as_str())
            {
                target.dialog = Some(
                    json!({"dialog_id":id,"attempted":false,"type":params["type"],"message":params["message"].as_str().unwrap_or("").chars().take(512).collect::<String>()}),
                );
            }
        }
        "Page.javascriptDialogClosed" => {
            if let Some(target) = targets
                .lock()
                .unwrap()
                .values_mut()
                .find(|t| Some(t.session.as_str()) == event["sessionId"].as_str())
            {
                target.dialog = None;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_dialog(
    cdp: &Cdp,
    targets: &Arc<Mutex<HashMap<String, Target>>>,
    target: &Target,
    id: &Value,
    accept: &Value,
    deadline: Instant,
) -> Result<()> {
    {
        let mut targets = targets.lock().unwrap();
        let dialog = targets
            .get_mut(&target.id)
            .and_then(|t| t.dialog.as_mut())
            .filter(|d| d["dialog_id"] == *id && d["attempted"] == false)
            .ok_or_else(|| Failure::new("browser_no_dialog"))?;
        dialog["attempted"] = true.into();
    }
    let result = cdp
        .call(
            Some(&target.session),
            "Page.handleJavaScriptDialog",
            json!({"accept":accept}),
            deadline,
        )
        .await;
    if result.as_ref().is_err_and(|e| e.state == "not_sent")
        && let Some(dialog) = targets
            .lock()
            .unwrap()
            .get_mut(&target.id)
            .and_then(|t| t.dialog.as_mut())
            .filter(|d| d["dialog_id"] == *id)
    {
        dialog["attempted"] = false.into();
    }
    result.map(|_| ())
}

fn validate(operation: &str, params: &Value) -> Result<()> {
    let map = params
        .as_object()
        .ok_or_else(|| Failure::new("browser_invalid_params"))?;
    let allowed: &[&str] = match operation {
        "browser_open" => &["url", "profile_name"],
        "browser_sessions" | "browser_profiles" | "browser_files" => &[],
        "browser_tabs" => &["session_id"],
        "browser_screenshot" => &["session_id", "tab_id", "control_epoch"],
        "browser_observe" => &[
            "session_id",
            "tab_id",
            "control_epoch",
            "max_nodes",
            "max_chars",
        ],
        "browser_act" => &["session_id", "tab_id", "control_epoch", "action"],
        "browser_wait" => &[
            "session_id",
            "tab_id",
            "control_epoch",
            "url_contains",
            "text_contains",
        ],
        "browser_control" => &[
            "session_id",
            "control_epoch",
            "action",
            "profile_name",
            "artifact_id",
        ],
        _ => return Err(Failure::new("browser_invalid_params")),
    };
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Failure::new("browser_invalid_params"));
    }
    if let Some(value) = params.get("profile_name")
        && !value.as_str().is_some_and(profiles::valid_name)
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    for key in ["session_id", "tab_id"] {
        if let Some(value) = map.get(key)
            && !value.as_str().is_some_and(|s| {
                !s.is_empty()
                    && s.len() <= 128
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    for (key, min, max) in [
        ("max_nodes", 1, 1000),
        ("max_chars", 128, 64000),
        ("control_epoch", 1, 9_007_199_254_740_991),
    ] {
        if let Some(value) = map.get(key)
            && !value.as_u64().is_some_and(|v| v >= min && v <= max)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    if let Some(url) = map.get("url")
        && !url
            .as_str()
            .is_some_and(|s| s.len() <= 4096 && page::url_allowed(s))
    {
        return Err(Failure::new("browser_url_denied"));
    }
    if operation == "browser_act" {
        let action = params["action"]
            .as_object()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let kind = action.get("kind").and_then(Value::as_str).unwrap_or("");
        let fields: &[&str] = match kind {
            "navigate" => &["kind", "url"],
            "fill" => &["kind", "ref", "text"],
            "press" => &["kind", "ref", "key"],
            "check" => &["kind", "ref", "checked"],
            "select" => &["kind", "ref", "value"],
            "attach_file" => &["kind", "ref", "file_grant_id"],
            "scroll" => &["kind", "delta_y"],
            "click" => &["kind", "ref"],
            "new_tab" | "close_tab" | "select_tab" => &["kind"],
            "dialog" => &["kind", "accept", "dialog_id"],
            _ => return Err(Failure::new("browser_invalid_params")),
        };
        if action.len() != fields.len() || action.keys().any(|key| !fields.contains(&key.as_str()))
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if kind == "navigate"
            && !action["url"]
                .as_str()
                .is_some_and(|s| s.len() <= 4096 && page::url_allowed(s))
        {
            return Err(Failure::new("browser_url_denied"));
        }
        if fields.contains(&"ref") && action["ref"].as_str().is_none_or(|s| s.len() > 128) {
            return Err(Failure::new("browser_invalid_params"));
        }
        if kind == "fill" && action["text"].as_str().is_none_or(|s| s.len() > 16384) {
            return Err(Failure::new("browser_invalid_params"));
        }
        if kind == "attach_file"
            && action["file_grant_id"].as_str().is_none_or(|s| {
                s.len() != 32
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if kind == "select"
            && action["value"]
                .as_str()
                .is_none_or(|s| s.chars().count() > 256)
            || kind == "scroll"
                && action["delta_y"]
                    .as_f64()
                    .is_none_or(|n| !n.is_finite() || n.abs() > 2000.)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if kind == "press"
            && !action["key"].as_str().is_some_and(|s| {
                [
                    "Enter",
                    "Tab",
                    "Escape",
                    "Backspace",
                    "ArrowDown",
                    "ArrowUp",
                    "ArrowLeft",
                    "ArrowRight",
                ]
                .contains(&s)
            })
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if (kind == "check" && !action["checked"].is_boolean())
            || (kind == "dialog" && !action["accept"].is_boolean())
            || (kind == "dialog"
                && action["dialog_id"]
                    .as_str()
                    .is_none_or(|s| s.is_empty() || s.len() > 128))
        {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    if operation == "browser_control"
        && !params["action"].as_str().is_some_and(|s| {
            ["pause", "resume", "close", "delete_profile", "delete_file"].contains(&s)
        })
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if operation == "browser_control"
        && ((params["action"] == "delete_profile"
            && (map.len() != 2 || !map.contains_key("profile_name")))
            || (params["action"] != "delete_profile" && map.contains_key("profile_name")))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if operation == "browser_control"
        && ((params["action"] == "delete_file"
            && (map.len() != 2
                || !params["artifact_id"]
                    .as_str()
                    .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))))
            || (params["action"] != "delete_file" && map.contains_key("artifact_id")))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if operation == "browser_wait" {
        let conditions: Vec<_> = ["url_contains", "text_contains"]
            .into_iter()
            .filter_map(|key| params.get(key))
            .collect();
        if conditions.len() != 1
            || !conditions[0]
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 1024)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    Ok(())
}
