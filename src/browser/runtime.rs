use super::{
    Failure, Principal, Result, Settings, Shell,
    control::Control,
    engine::{
        Context, DocumentRequest, Engine, Event, FrameId, Navigation, NetworkId, RequestDecision,
        SessionPort, TabId, TransferPort,
    },
    files::{self, Files},
    page::{self, Element},
    process::{self, BrowserProcess},
    profiles::{self, Profiles},
    sites::{Lookup, Resolver, Sites, Verdict},
};
use crate::browser::engine::ContextKind;
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
#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "experimental-browser-bidi"
))]
mod v2;
use human::HumanLease;

const MAX_SESSIONS: usize = 2;
const MAX_TABS: usize = 8;
const IDLE: Duration = Duration::from_secs(600);

#[derive(Clone)]
struct Target {
    id: String,
    handle: TabId,
    root: FrameId,
    session: Context,
    kind: ContextKind,
    url: String,
    title: String,
    dialog: Option<Value>,
    ready: bool,
    /// Set while the tab shows the outcome of a navigation of its own document
    /// that the site policy refused. The value says whether a request had left
    /// the browser before the refusal.
    blocked: Option<bool>,
}
struct Slot {
    owner: String,
    user: String,
    profile_name: Option<String>,
    control: Arc<Control>,
    closing: AtomicBool,
    transport: Mutex<Option<Engine>>,
    session: AsyncMutex<Session>,
}
struct Session {
    process: Option<BrowserProcess>,
    targets: Arc<Mutex<HashMap<String, Target>>>,
    references: HashMap<String, Element>,
    read: Option<PageRead>,
    active: String,
    observed_tab: String,
    touched: Instant,
    human: Option<HumanLease>,
    guard: Option<tokio::task::JoinHandle<()>>,
}
struct PageRead {
    token: String,
    tab: String,
    epoch: u64,
    until: Instant,
    cursor: page::Cursor,
}

#[tokio::test]
async fn cancelled_close_retains_event_task_ownership() {
    let done = Arc::new(tokio::sync::Notify::new());
    let signal = done.clone();
    let mut session = Session {
        process: None,
        targets: Arc::new(Mutex::new(HashMap::new())),
        references: HashMap::new(),
        read: None,
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
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    ))]
    v2_sessions: Arc<Mutex<HashMap<String, Arc<v2::Slot>>>>,
    settings: Settings,
    executable: Option<PathBuf>,
    root: Option<PathBuf>,
    sessions: Arc<Mutex<HashMap<String, Arc<Slot>>>>,
    initialized: Mutex<bool>,
    admission: Mutex<Admission>,
    profiles: Profiles,
    root_lock: Mutex<Option<Arc<crate::profile_lock::ProfileLocks>>>,
    files: Arc<Files>,
    gate: Arc<Gate>,
    shell: Shell,
}

/// The site policy, as the request hook of every session applies it.
#[derive(Default)]
struct Gate {
    sites: Sites,
    lookup: Lookup,
}

impl Browsers {
    pub fn new(settings: &Settings, config_path: Option<&Path>, token: &str, shell: Shell) -> Self {
        let root = config_path.and_then(|config| super::storage_root(config, token));
        let files = Files::new(root.as_deref());
        Self {
            #[cfg(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            ))]
            v2_sessions: Arc::new(Mutex::new(HashMap::new())),
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
            gate: Arc::new(Gate {
                sites: settings.sites.clone(),
                lookup: Lookup::default(),
            }),
            shell,
        }
    }
    /// Replaces the lookup of host names. Tests use it to say where a name leads.
    pub fn resolve_with(&self, resolver: Resolver) {
        self.gate.lookup.resolve_with(resolver);
    }
    /// Refuses, before anything is done, an address that the site policy rules
    /// out as it is written.
    fn admit(&self, address: &Value) -> Result<()> {
        match address.as_str().map(url::Url::parse) {
            Some(Ok(address)) if self.gate.sites.judge(&address) == Verdict::Deny => {
                Err(Failure::new("browser_site_denied"))
            }
            _ => Ok(()),
        }
    }
    pub fn info(&self) -> Value {
        let reason = if !self.settings.enabled {
            Some("disabled_by_local_policy")
        } else if let Some(reason) = self.settings.shell_coexistence.refuses(self.shell) {
            Some(reason)
        } else if self.root.is_none() {
            Some("private_storage_unavailable")
        } else if self.executable.is_none() {
            Some("browser_not_installed")
        } else {
            None
        };
        let mut value = json!({"version":1,"available":reason.is_none(),"reason":reason,"mode":"managed_headless","profiles":if self.settings.persistent_profiles {vec!["temporary","named"]}else{vec!["temporary"]},"max_sessions":MAX_SESSIONS,"max_tabs":MAX_TABS,"idle_seconds":IDLE.as_secs(),"principal_version":1,"network_isolation":false,"sites":self.settings.sites.summary(),"shell":{"enabled":self.shell.enabled,"sandboxed":self.shell.sandboxed,"coexistence":self.settings.shell_coexistence},"human_takeover":true,"screenshot":true,"file_transfer":self.settings.downloads||self.settings.uploads,"downloads":{"enabled":self.settings.downloads,"http_attachment_only":true,"declared_length_required":true,"max_file_bytes":files::MAX_FILE,"max_total_bytes":files::MAX_TOTAL,"retention_seconds":files::RETENTION},"uploads":{"enabled":self.settings.uploads,"source":"user_local_file","main_frame_only":true,"max_file_bytes":files::MAX_FILE,"grant_ttl_seconds":files::UPLOAD_TTL,"uses":1}});
        value["v2"] = super::selection::capability(
            &self.settings,
            reason.filter(|r| *r != "browser_not_installed"),
            self.executable.is_some(),
        );
        value
    }
    fn prepare(&self) -> Result<&Path> {
        if self.info()["available"] != true {
            return Err(Failure::new("browser_unavailable"));
        }
        self.prepare_storage()
    }
    fn prepare_storage(&self) -> Result<&Path> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| Failure::new("browser_unavailable"))?;
        let mut initialized = self.initialized.lock().unwrap();
        if !*initialized {
            let parent = root.parent().unwrap();
            fn prepare_directory(path: &Path) -> Result<()> {
                match crate::storage::create_private_dir(path) {
                    Ok(_) => Ok(()),
                    Err(crate::error::Error::Io(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        crate::storage::private_metadata(path, true)
                            .map(|_| ())
                            .map_err(super::io_error)
                    }
                    Err(error) => Err(super::io_error(error)),
                }
            }
            prepare_directory(parent)?;
            let lock_path = super::storage_lock(root);
            crate::profile_lock::ProfileLocks::recover(std::slice::from_ref(&lock_path))
                .map_err(|_| Failure::new("browser_profile_busy"))?;
            let lock = crate::profile_lock::ProfileLocks::acquire(&[lock_path])
                .map_err(|_| Failure::new("browser_profile_busy"))?;
            prepare_directory(root)?;
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
        if operation == "browser_open"
            && let Some(selection) = super::selection::parse(params)?
        {
            #[cfg(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            ))]
            {
                return self.v2_open(params, principal, selection, deadline).await;
            }
            #[cfg(not(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            )))]
            {
                let _ = selection;
                return Err(Failure::new("browser_backend_unavailable"));
            }
        }
        #[cfg(all(
            target_os = "linux",
            target_arch = "x86_64",
            feature = "experimental-browser-bidi"
        ))]
        {
            if self.v2_contains(params) {
                return self
                    .v2_execute(operation, params, principal, deadline)
                    .await;
            }
            if operation == "browser_sessions"
                && self.info()["available"] != true
                && self.info()["v2"]["available"] == true
            {
                return Ok(json!({"sessions":self.v2_listing(principal,false)}));
            }
        }
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
                    Ok(s) if s.process.as_ref().is_some_and(|p| p.engine.connected()) => "ready",
                    Ok(s) if s.process.is_none() => "closed",
                    Ok(_) => "lost",
                    Err(_) => "busy",
                };
                sessions.push(json!({"session_id":id,"state":state,"control_epoch":epoch,"controller":if paused {"paused"} else {"ai"},"profile_name":slot.profile_name}));
            }
            #[cfg(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            ))]
            sessions.extend(self.v2_listing(principal, false));
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
        if !process.engine.connected() {
            return Err(Failure::unknown("browser_lost"));
        }
        if process.engine.pending() != 0 && operation != "browser_tabs" && !resolving_dialog {
            return Err(Failure::unknown("browser_input_pending"));
        }
        let engine = if paused {
            process.engine.human(slot.control.clone(), epoch)
        } else {
            process.engine.authorized(slot.control.clone(), epoch)
        };
        let (engine, effects) = engine.track_effects();
        if operation == "browser_tabs" {
            let tabs: Vec<_> = session
                .targets
                .lock()
                .unwrap()
                .values()
                .filter(|t| t.kind == ContextKind::Page)
                .map(|t| {
                    let mut tab = json!({"tab_id":t.id,"url":t.url,"title":t.title,"dialog":t.dialog,"ready":t.ready});
                    if t.blocked.is_some() {
                        tab["blocked"] = "site_policy".into();
                    }
                    tab
                })
                .collect();
            return Ok(
                json!({"session_id":id,"control_epoch":epoch,"active_tab":session.active,"tabs":tabs,"controller":if paused{"paused"}else{"ai"},"input_pending":engine.pending()!=0}),
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
            .filter(|t| t.kind == ContextKind::Page)
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
                let before = page::frame_tree(&engine, &target.session, deadline).await?;
                let mut value = super::image::capture(&engine, &target.session, deadline).await?;
                let after = page::frame_tree(&engine, &target.session, deadline).await?;
                if before.first().map(|f| (&f.id, &f.document)) != after.first().map(|f| (&f.id, &f.document)) {
                    return Err(Failure::new("browser_stale_frame"));
                }
                value["page"] = page::metadata(&engine,after.first().ok_or_else(||Failure::new("browser_tab_unavailable"))?,deadline).await?;
                value["page"]["tab_id"] = tab.clone().into();
                mark_blocked(&mut value, &session.targets, &tab);
                Ok(value)
            }
            "browser_observe" => {
                if target.dialog.is_some() {
                    return Err(Failure::new("browser_dialog_open"));
                }
                let mut tree = frames(&engine, &session, &target, deadline).await?;
                let continuing = params["cursor"].is_string();
                if let Some(token)=params["cursor"].as_str()
                    && !session.read.as_ref().is_some_and(|r|r.token==token&&r.tab==tab&&r.epoch==epoch&&r.until>Instant::now()&&r.cursor.matches(&tree)) {
                    return Err(Failure::new("browser_stale_cursor"));
                }
                let next_token=crate::random::hex_token::<16>().map_err(super::io_error)?;
                let previous=session.read.take();
                let mut read=if continuing {
                    let mut read=previous.unwrap();
                    read.cursor.compare(&tree);
                    read
                } else {
                    if let Some(previous)=previous {previous.cursor.close(&engine,deadline).await;}
                    PageRead{token:String::new(),tab:tab.clone(),epoch,until:Instant::now()+Duration::from_secs(300),cursor:page::Cursor::new(&tree, scope(params))?}
                };
                // A first read that finds the document replaced under it starts once more,
                // on the document that is there now. Reading sends the page nothing, so
                // nothing is repeated. A continuation has no earlier part that would go
                // with a new document, so it is not started again.
                let mut again = !continuing;
                let (mut value, refs) = loop {
                    let result=async {
                        let (mut value, refs) = page::observe_next(
                            &engine,
                            &mut read.cursor,
                            params["max_nodes"].as_u64().unwrap_or(200) as usize,
                            params["max_chars"].as_u64().unwrap_or(20000) as usize,
                            deadline,
                        )
                        .await?;
                        value["page"] = page::metadata(&engine,tree.first().ok_or_else(||Failure::new("browser_tab_unavailable"))?,deadline).await?;
                        value["page"]["tab_id"] = tab.clone().into();
                        mark_loading(&mut value);
                        mark_blocked(&mut value, &session.targets, &tab);
                        Ok::<_,Failure>((value,refs))
                    }.await;
                    let error = match result {
                        Ok(value) => break value,
                        Err(error) => error,
                    };
                    read.cursor.close(&engine,Instant::now()+Duration::from_millis(500)).await;
                    if again
                        && matches!(error.code, "browser_stale_reference" | "browser_command_failed")
                        && Instant::now() + Duration::from_secs(3) < deadline
                        && let Ok(fresh) = frames(&engine, &session, &target, deadline).await
                        && let Ok(cursor) = page::Cursor::new(&fresh, scope(params))
                    {
                        again = false;
                        tree = fresh;
                        read.cursor = cursor;
                        continue;
                    }
                    session.references.clear();
                    return Err(error);
                };
                session.references = refs;
                session.observed_tab = tab.clone();
                if read.cursor.more() {
                    read.token=next_token;
                    read.until=Instant::now()+Duration::from_secs(300);
                    value["next_cursor"]=read.token.clone().into();
                    session.read=Some(read);
                } else {read.cursor.close(&engine,deadline).await;}
                Ok(value)
            }
            "browser_act" => {
                if let Some(read)=session.read.take() {read.cursor.close(&engine,deadline).await;}
                let action = &params["action"];
                match action["kind"].as_str().unwrap_or("") {
                    "scroll" => {
                        if target.dialog.is_some() { return Err(Failure::new("browser_dialog_open")); }
                        let tree = page::frame_tree(&engine, &target.session, deadline).await?;
                        let frame = tree.first().ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
                        session.references.clear();
                        page::scroll(&engine, frame, &action["delta_y"], deadline).await
                    }
                    "navigate" => {
                        self.admit(&action["url"])?;
                        session.references.clear();
                        if let Some(target) = session.targets.lock().unwrap().get_mut(&tab) {
                            target.blocked = None;
                        }
                        let reply = navigate(&engine, &target.session, &action["url"], deadline).await?;
                        if reply.failed {
                            Err(refusal(&session.targets, &tab).unwrap_or_else(|| Failure::unknown("browser_navigation_failed")))
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
                            .filter(|t| t.kind == ContextKind::Page)
                            .count()
                            >= MAX_TABS
                        {
                            return Err(Failure::new("browser_tab_limit"));
                        }
                        let new_tab = engine.new_tab(deadline).await?.v1().to_owned();
                        wait_target(&engine, &session.targets, &new_tab, deadline).await?;
                        session.active = new_tab.clone();
                        Ok(json!({"tab_id":new_tab}))
                    }
                    "close_tab" => {
                        if session
                            .targets
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|t| t.kind == ContextKind::Page)
                            .count()
                            <= 1
                        {
                            return Err(Failure::new("browser_last_tab"));
                        }
                        engine.close_tab(&target.handle, deadline).await?;
                        session.targets.lock().unwrap().remove(&tab);
                        if session.active == tab {
                            let next_active = session
                                .targets
                                .lock()
                                .unwrap()
                                .values()
                                .find(|t| t.kind == ContextKind::Page && t.ready)
                                .map(|t| t.id.clone())
                                .unwrap_or_default();
                            session.active = next_active;
                        }
                        session.references.clear();
                        Ok(json!({"tab_closed":true}))
                    }
                    "select_tab" => {
                        engine.activate(&target.session,deadline).await?;
                        session.active = tab;
                        session.references.clear();
                        Ok(json!({"active_tab":session.active}))
                    }
                    "dialog" => {
                        handle_dialog(&engine,&session.targets,&target,&action["dialog_id"],&action["accept"],deadline).await?;
                        Ok(json!({"dialog_handled":true,"controller":if paused{"paused"}else{"ai"},"input_pending":engine.pending()!=0}))
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
                            let tree = page::frame_tree(&engine, &target.session, deadline).await?;
                            let top = tree.first().ok_or_else(||Failure::new("browser_tab_unavailable"))?;
                            let origin = element.prepare_file(&engine, top, deadline).await?;
                            let path = self.files.consume_upload(&files::Owner{principal:principal.clone(),session:id.into(),tab:tab.clone(),origin,control:slot.control.clone()},action["file_grant_id"].as_str().unwrap_or(""),operation_id)?;
                            element.attach_file(&engine, &path, deadline).await.map(|mut value|{value["file_grant_id"]=action["file_grant_id"].clone();value})
                        } else { element.act(&engine, action, deadline).await };
                        session.references.clear();
                        result
                    }
                }
            }
            "browser_wait" => {
                if let Some(read)=session.read.take() {read.cursor.close(&engine,deadline).await;}
                let expected = params["url_contains"].as_str().unwrap_or("");
                let text = params["text_contains"].as_str();
                loop {
                    // A changing document is normal while waiting for a postcondition, and a
                    // read that the browser rejects during navigation has sent nothing.
                    // Retry observation only; never replay the preceding action.
                    let changing = |error: &Failure| matches!(error.code, "browser_stale_reference" | "browser_command_failed");
                    match frames(&engine, &session, &target, deadline).await {
                        Ok(tree) => if let Some(text) = text {
                            match page::observe(&engine, &tree, 200, 20000, deadline).await {
                                Ok((value,_)) => if value["nodes"].as_array().is_some_and(|v|v.iter().any(|n|n["name"].as_str().is_some_and(|s|s.contains(text)))) {
                                    break Ok(json!({"condition_observed":true,"observation_id":value["observation_id"],"coverage":value["coverage"]}));
                                },
                                Err(error) if changing(&error) => {},
                                Err(error) => return Err(error),
                            }
                        // The address as the page sees it, with its fragment: a link within
                        // a page and a hash route change nothing else.
                        } else if let Some(url) = tree.first().map(|frame| format!("{}{}", frame.url, frame.fragment)).filter(|url| url.contains(expected)) {
                            break Ok(json!({"condition_observed":true,"url":url}));
                        },
                        Err(error) if changing(&error) => {},
                        Err(error) => return Err(error),
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
        let outcome =
            outcome.map_err(|error| settle(operation, effects.load(Ordering::Acquire), error));
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
        self.admit(&params["url"])?;
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
                read: None,
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
            let occupied = sessions.len();
            #[cfg(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            ))]
            let occupied = occupied + self.v2_sessions.lock().unwrap().len();
            if occupied >= MAX_SESSIONS {
                return Err(Failure::new("browser_session_limit"));
            }
            sessions.insert(id.clone(), slot.clone());
        }
        let mut session = slot.session.lock().await;
        let result=async {
            let named = profile_name.map(|name|self.profiles.open(self.root.as_deref().unwrap(),principal,name)).transpose()?;
            let process=BrowserProcess::launch(self.executable.as_deref().unwrap(),self.root.as_deref().unwrap(),&id,named,deadline).await?;
            let engine=process.engine.authorized(slot.control.clone(), 1);
            *slot.transport.lock().unwrap()=Some(engine.clone());
            session.process=Some(process);
            if slot.closing.load(Ordering::Acquire) || slot.control.snapshot() != (1, false) {
                engine.stop();
                return Err(Failure::new("browser_control_changed"));
            }
            session.guard=Some(guard(engine.clone(),session.targets.clone(),self.settings.downloads.then(||DownloadContext{files:self.files.clone(),principal:principal.clone(),session:id.clone(),control:slot.control.clone()}),self.gate.clone()));
            engine.initialize(deadline).await?;
            let browser=engine.product(deadline).await.ok().flatten().as_deref().and_then(product);
            let target=loop {
                if let Some(target)=session.targets.lock().unwrap().values().find(|t|t.kind==ContextKind::Page&&t.ready).cloned(){break target;}
                if !engine.connected() || slot.closing.load(Ordering::Acquire) || slot.control.snapshot() != (1, false) { return Err(Failure::unknown("browser_control_changed")); }
                if Instant::now()>=deadline {return Err(Failure::unknown("browser_timeout_unknown"));}
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            session.active=target.id.clone();
            if let Some(url)=params["url"].as_str(){
                let reply=navigate(&engine,&target.session,&json!(url),deadline).await?;
                // A session whose first page the site policy refused has nothing to show.
                if reply.failed && let Some(refused)=refusal(&session.targets,&target.id) {
                    return Err(refused);
                }
            }
            if slot.control.snapshot() != (1, false) { return Err(Failure::unknown("browser_control_changed")); }
            let mut opened=json!({"session_id":id,"tab_id":target.id,"control_epoch":1,"state":"ready","profile":if profile_name.is_some(){"named"}else{"temporary"},"profile_name":profile_name,"profile_id":profile_name.map(|name|profiles::id(principal,name))});
            if let Some(browser)=browser { opened["browser"]=browser; }
            Ok(opened)
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
            if let Some(engine) = slot.transport.lock().unwrap().as_ref() {
                engine.graceful_stop();
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
                if !process.engine.connected() {
                    return Err(Failure::unknown("browser_lost"));
                }
                if process.engine.pending() != 0 {
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
        #[cfg(all(
            target_os = "linux",
            target_arch = "x86_64",
            feature = "experimental-browser-bidi"
        ))]
        self.v2_pause_all(true);
        for slot in self.sessions.lock().unwrap().values() {
            slot.control.change(true);
            slot.closing.store(true, Ordering::Release);
            if let Some(engine) = slot.transport.lock().unwrap().as_ref() {
                engine.graceful_stop();
            }
        }
    }
    pub fn pause_all(&self) {
        let mut admission = self.admission.lock().unwrap();
        admission.online = false;
        #[cfg(all(
            target_os = "linux",
            target_arch = "x86_64",
            feature = "experimental-browser-bidi"
        ))]
        self.v2_pause_all(false);
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
        #[cfg(all(
            target_os = "linux",
            target_arch = "x86_64",
            feature = "experimental-browser-bidi"
        ))]
        self.v2_expire_offline();
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
            if let Some(engine) = slot.transport.lock().unwrap().as_ref() {
                engine.graceful_stop();
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
        #[cfg(all(
            target_os = "linux",
            target_arch = "x86_64",
            feature = "experimental-browser-bidi"
        ))]
        {
            stopped &= self.v2_close_all().await;
        }
        self.files.wait().await;
        stopped
    }
    // Runtime has joined all request workers and browser-owned tasks before this call.
    pub fn release_stopped_storage(&self) {
        self.files.release_stopped_storage();
        self.root_lock.lock().unwrap().take();
    }
}

async fn wait_target(
    engine: &Engine,
    targets: &Arc<Mutex<HashMap<String, Target>>>,
    id: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        if !engine.connected() {
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
/// Starts a navigation and waits for the browser's answer, which comes when the
/// new document commits or the load fails. A host that never answers would keep
/// the command, and with it the whole session, pending until the browser gives
/// up minutes later. Stop waiting early, cancel the load and let the command
/// settle, so that the session can be used again at once.
async fn navigate(
    engine: &Engine,
    session: &Context,
    url: &Value,
    deadline: Instant,
) -> Result<Navigation> {
    let give_up = navigation_deadline(Instant::now(), deadline);
    match engine
        .navigate(session, url.as_str().unwrap_or(""), give_up)
        .await
    {
        Err(error) if error.code == "browser_timeout_unknown" => {
            let _ = engine.stop_loading(session, deadline).await;
            while engine.pending() != 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(Failure::unknown("browser_navigation_failed"))
        }
        reply => reply,
    }
}

/// When to stop waiting for a navigation: early enough to cancel it and let the
/// browser settle before the call's own deadline.
fn navigation_deadline(now: Instant, deadline: Instant) -> Instant {
    let remaining = deadline.saturating_duration_since(now);
    deadline - (remaining / 4).min(Duration::from_secs(3))
}

/// A document that the browser is still parsing has more to come, however much of
/// it was read. Say so, instead of reporting the part as the complete page.
fn mark_loading(observation: &mut Value) {
    if observation["page"]["ready_state"] != "loading" {
        return;
    }
    observation["coverage"]["complete_within_scope"] = false.into();
    if let Some(omitted) = observation["coverage"]["omitted"].as_array_mut()
        && !omitted.iter().any(|reason| reason == "document_loading")
    {
        omitted.push("document_loading".into());
    }
}

/// Says on a result's page that the tab shows the outcome of a navigation the
/// site policy refused, so that an empty page explains itself.
fn mark_blocked(result: &mut Value, targets: &Mutex<HashMap<String, Target>>, tab: &str) {
    if targets
        .lock()
        .unwrap()
        .get(tab)
        .is_some_and(|target| target.blocked.is_some())
    {
        result["page"]["blocked"] = "site_policy".into();
    }
}

/// Fails a paused request the way a content blocker would.
async fn refuse(
    engine: &impl SessionPort,
    params: &DocumentRequest,
    deadline: Instant,
) -> Result<()> {
    engine
        .respond(&params.handle, RequestDecision::Deny, deadline)
        .await
}

/// Lets a paused document go on, or takes it over as a download.
async fn release(
    engine: &impl TransferPort,
    targets: &Mutex<HashMap<String, Target>>,
    downloads: Option<&DownloadContext>,
    session: Option<&Context>,
    params: &DocumentRequest,
    deadline: Instant,
) -> Result<()> {
    if let Some(downloads) = downloads
        && params.headers.as_ref().is_some_and(|headers| {
            headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("content-disposition")
                    && h.value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("attachment")
            })
        })
    {
        let target = targets
            .lock()
            .unwrap()
            .values()
            .find(|t| Some(&t.session) == session)
            .cloned()
            .ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
        let origin = url::Url::parse(params.url.as_str())
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
            .intercept(engine.clone(), params, owner)
            .is_err()
        {
            engine
                .respond(&params.handle, RequestDecision::Abort, deadline)
                .await?;
        }
        return Ok(());
    }
    engine
        .respond(&params.handle, RequestDecision::Continue, deadline)
        .await
}

/// Records on a tab what became of the latest request for its own document:
/// refused by the site policy, or under way.
fn note(
    targets: &Mutex<HashMap<String, Target>>,
    session: Option<&Context>,
    frame: &FrameId,
    blocked: Option<bool>,
) {
    if let Some(target) =
        targets.lock().unwrap().values_mut().find(|t| {
            t.kind == ContextKind::Page && Some(&t.session) == session && t.root == *frame
        })
    {
        target.blocked = blocked;
    }
}

/// The failure to report when the site policy refused the tab's own navigation.
/// Nothing was sent when the first request was refused. After a redirect or a
/// response, a request has left the browser and the outcome is uncertain.
fn refusal(targets: &Mutex<HashMap<String, Target>>, tab: &str) -> Option<Failure> {
    let sent = targets.lock().unwrap().get(tab)?.blocked?;
    Some(if sent {
        Failure::unknown("browser_site_denied")
    } else {
        Failure::new("browser_site_denied")
    })
}

/// Splits the product string a browser reports about itself, such as
/// `Chrome/122.0.6261.128`, into a bounded name and a dotted version. A string
/// of any other shape is not reported.
fn product(value: &str) -> Option<Value> {
    let (name, version) = value.split_once('/')?;
    let named =
        !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric());
    let versioned = version.len() <= 32
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    (named && versioned).then(|| json!({"product":name,"version":version}))
}

/// Chooses the execution state reported for a failed call. Once an effect
/// command has left, any `browser_act` failure is uncertain. A command that the
/// browser rejected has changed nothing when no effect command left during the
/// call, so it is reported as not sent and does not pause the session.
fn settle(operation: &str, effects: bool, mut error: Failure) -> Failure {
    // A refusal by the site policy says for itself whether anything left the browser.
    if error.code == "browser_site_denied" {
        return error;
    }
    if operation == "browser_act" && effects {
        error.state = "unknown";
    } else if !effects && error.code == "browser_command_failed" {
        error.state = "not_sent";
    }
    error
}

async fn frames(
    engine: &Engine,
    _session: &Session,
    target: &Target,
    deadline: Instant,
) -> Result<Vec<page::Frame>> {
    engine.page_frames(&target.session, deadline).await
}

/// What the browser said about the address behind recent responses, one entry
/// for a request, kept until it is asked for. The oldest give way to new ones.
#[derive(Default)]
struct Spaces {
    known: HashMap<NetworkId, bool>,
    order: std::collections::VecDeque<NetworkId>,
}
impl Spaces {
    fn record(&mut self, request: NetworkId, public: bool) {
        if self.known.insert(request.clone(), public).is_none() {
            self.order.push_back(request);
            if self.order.len() > 1024
                && let Some(oldest) = self.order.pop_front()
            {
                self.known.remove(&oldest);
            }
        }
    }
    fn take(&mut self, request: &NetworkId) -> Option<bool> {
        self.known.remove(request)
    }
}

/// What one session's event task has under way.
#[derive(Default)]
struct Work {
    initializers: tokio::task::JoinSet<()>,
    /// Requests that wait for a host name to be looked up, or for the browser's
    /// word on a connection.
    checks: tokio::task::JoinSet<()>,
    spaces: Arc<Mutex<Spaces>>,
}

#[derive(Clone)]
struct DownloadContext {
    files: Arc<Files>,
    principal: Principal,
    session: String,
    control: Arc<Control>,
}

fn guard(
    engine: Engine,
    targets: Arc<Mutex<HashMap<String, Target>>>,
    downloads: Option<DownloadContext>,
    gate: Arc<Gate>,
) -> tokio::task::JoinHandle<()> {
    let mut events = engine.events();
    tokio::spawn(async move {
        let mut work = Work::default();
        loop {
            if !engine.connected() {
                break;
            }
            let event = tokio::select! {
                joined=work.initializers.join_next(), if !work.initializers.is_empty()=>{
                    if joined.is_some_and(|r|r.is_err()){engine.stop();break;}
                    continue;
                },
                joined=work.checks.join_next(), if !work.checks.is_empty()=>{
                    if joined.is_some_and(|r|r.is_err()){engine.stop();break;}
                    continue;
                },
                event=events.recv()=>match event {Ok(event)=>event,Err(_)=>{engine.stop();break;}},
                _=tokio::time::sleep(Duration::from_millis(500))=>{if !engine.connected(){break;}else{continue;}},
            };
            if handle_event(
                &engine,
                &targets,
                downloads.as_ref(),
                &gate,
                &mut work,
                event,
            )
            .await
            .is_err()
            {
                if engine.connected() {
                    engine.stop();
                }
                break;
            }
        }
        work.initializers.abort_all();
        work.checks.abort_all();
        while work.initializers.join_next().await.is_some() {}
        while work.checks.join_next().await.is_some() {}
    })
}
async fn handle_event(
    engine: &impl TransferPort,
    targets: &Arc<Mutex<HashMap<String, Target>>>,
    downloads: Option<&DownloadContext>,
    gate: &Arc<Gate>,
    work: &mut Work,
    event: Event,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    match event {
        Event::Attached {
            context: session,
            info,
        } => {
            let id = info.tab.v1();
            let kind = info.kind;
            if kind == ContextKind::Background {
                if work.initializers.len() >= 40 {
                    return Err(Failure::new("browser_busy"));
                }
                let (engine, session) = (engine.clone(), session.clone());
                work.initializers.spawn(async move {
                    // Resuming a worker can itself wait for a page request.
                    // Keep Fetch processing available to resolve that dependency,
                    // just as for page/iframe initialization below.
                    if engine
                        .resume_context(&session, Instant::now() + Duration::from_secs(125))
                        .await
                        .is_err()
                    {
                        engine.stop();
                    }
                });
                return Ok(());
            }
            if (kind == ContextKind::Page
                && targets
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|t| t.kind == ContextKind::Page)
                    .count()
                    >= MAX_TABS)
                || targets.lock().unwrap().len() >= 40
                || work.initializers.len() >= 40
            {
                engine.discard_tab(&info.tab, deadline).await?;
                return Ok(());
            }
            targets.lock().unwrap().insert(
                id.into(),
                Target {
                    id: id.into(),
                    handle: info.tab.clone(),
                    root: info.root.clone(),
                    session: session.clone(),
                    kind,
                    url: info.url,
                    title: info.title,
                    dialog: None,
                    ready: false,
                    blocked: None,
                },
            );
            let (engine, targets, id, session) = (
                engine.clone(),
                targets.clone(),
                id.to_owned(),
                session.clone(),
            );
            let downloads = downloads.is_some();
            let responses = downloads || gate.sites.strict();
            work.initializers.spawn(async move {
                // A popup can have no renderer document until its attachment
                // finishes. The event pump must keep serving Fetch meanwhile.
                let result = engine
                    .initialize_context(
                        &session,
                        downloads,
                        responses,
                        Instant::now() + Duration::from_secs(125),
                    )
                    .await;
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
                        if engine
                            .discard_tab(&info.tab, Instant::now() + Duration::from_secs(1))
                            .await
                            .is_err()
                            && engine.connected()
                        {
                            engine.stop();
                        }
                        targets.lock().unwrap().remove(&id);
                    }
                }
            });
        }
        Event::Detached(context) => {
            targets.lock().unwrap().retain(|_, t| t.session != context);
        }
        Event::TargetChanged(info) => {
            if let Some(target) = targets.lock().unwrap().get_mut(info.tab.v1()) {
                target.url = info.url;
                target.title = info.title;
                if target.url == "about:blank" {
                    target.blocked = None;
                }
            }
        }
        Event::DocumentPaused(params) => {
            let url = params.url.as_str();
            let ordinary = page::url_allowed(url);
            // What the address itself and the owner's lists say.
            let address = url::Url::parse(url).ok();
            let mut verdict = match &address {
                _ if url == "about:blank" => Verdict::Allow,
                Some(address) if ordinary => gate.sites.judge(address),
                _ => Verdict::Deny,
            };
            let responded = params.responded;
            let sent = responded || params.redirected;
            let session = params.context.as_ref();
            let frame = &params.frame;
            let request = &params.network;
            // What the browser said about this request before it set out is about an
            // earlier hop of a redirect.
            if gate.sites.strict() && !responded {
                work.spaces.lock().unwrap().take(request);
            }
            // A host name is judged by the addresses it leads to, before its request
            // is sent. The lookup must not hold up the events behind this one, so a
            // name that has to be asked about is answered from a task of its own.
            if verdict == Verdict::Resolve
                && !responded
                && let Some(host) = address.as_ref().and_then(|address| address.host_str())
            {
                let refuses = |found: &[std::net::IpAddr]| !gate.sites.judge_addresses(found);
                match gate.lookup.remembered(host) {
                    Some(answer) => {
                        if answer.is_some_and(|found| refuses(&found)) {
                            verdict = Verdict::Deny;
                        }
                    }
                    None if work.checks.len() < 64 => {
                        note(targets, session, frame, None);
                        let (engine, gate, targets) =
                            (engine.clone(), gate.clone(), targets.clone());
                        let (host, frame) = (host.to_owned(), frame.to_owned());
                        let session = session.cloned();
                        let request = params.handle.clone();
                        work.checks.spawn(async move {
                            let refused = gate
                                .lookup
                                .addresses(&host)
                                .await
                                .is_some_and(|found| !gate.sites.judge_addresses(&found));
                            if refused {
                                note(&targets, session.as_ref(), &frame, Some(sent));
                            }
                            let decision = if refused {
                                RequestDecision::Deny
                            } else {
                                RequestDecision::Continue
                            };
                            let answered = Instant::now() + Duration::from_secs(3);
                            let _ = engine.respond(&request, decision, answered).await;
                        });
                        return Ok(());
                    }
                    // Too many names are being asked about at once.
                    None => {
                        engine
                            .respond(&params.handle, RequestDecision::Deny, deadline)
                            .await?;
                        return Ok(());
                    }
                }
            }
            // Where the owner refuses private networks, the address the browser really
            // reached decides, whatever the name's lookup said.
            if gate.sites.strict() && verdict == Verdict::Resolve && params.status.is_some() {
                let reported = work.spaces.lock().unwrap().take(request);
                match reported {
                    Some(space) => {
                        if !space {
                            verdict = Verdict::Deny;
                        }
                    }
                    None if work.checks.len() < 64 => {
                        let (engine, gate, targets) =
                            (engine.clone(), gate.clone(), targets.clone());
                        let (spaces, downloads) = (work.spaces.clone(), downloads.cloned());
                        let host = address
                            .as_ref()
                            .and_then(|address| address.host_str())
                            .unwrap_or("")
                            .to_owned();
                        let (session, frame) = (session.cloned(), frame.to_owned());
                        let (request, params) = (request.to_owned(), params.clone());
                        work.checks.spawn(async move {
                            let session = session.as_ref();
                            // The browser's word on this connection has not arrived yet.
                            let mut reported = None;
                            for _ in 0..15 {
                                reported = spaces.lock().unwrap().take(&request);
                                if reported.is_some() {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                            let refused = match reported {
                                Some(space) => !space,
                                // Nothing was said, as for a page out of the browser's
                                // cache: what the name's lookup says stands.
                                None => gate
                                    .lookup
                                    .addresses(&host)
                                    .await
                                    .is_some_and(|found| !gate.sites.judge_addresses(&found)),
                            };
                            let deadline = Instant::now() + Duration::from_secs(3);
                            let done = if refused {
                                note(&targets, session, &frame, Some(true));
                                refuse(&engine, &params, deadline).await
                            } else {
                                let downloads = downloads.as_ref();
                                release(&engine, &targets, downloads, session, &params, deadline)
                                    .await
                            };
                            if done.is_err() && engine.connected() {
                                engine.stop();
                            }
                        });
                        return Ok(());
                    }
                    // Too many requests are waiting to be judged.
                    None => verdict = Verdict::Deny,
                }
            }
            let allowed = verdict != Verdict::Deny;
            if !allowed || !responded {
                // A refusal by the site policy stays on the tab until its document
                // sets out again.
                note(
                    targets,
                    session,
                    frame,
                    (!allowed && ordinary).then_some(sent),
                );
            }
            if allowed {
                release(engine, targets, downloads, session, &params, deadline).await?;
            } else {
                refuse(engine, &params, deadline).await?;
            }
        }
        Event::Connection { network, public } if gate.sites.strict() => {
            work.spaces.lock().unwrap().record(network, public);
        }
        Event::DialogOpened {
            context,
            kind,
            message,
        } => {
            let id = crate::random::hex_token::<16>().map_err(super::io_error)?;
            if let Some(target) = targets
                .lock()
                .unwrap()
                .values_mut()
                .find(|t| Some(&t.session) == context.as_ref())
            {
                target.dialog =
                    Some(json!({"dialog_id":id,"attempted":false,"type":kind,"message":message}));
            }
        }
        Event::DialogClosed { context } => {
            if let Some(target) = targets
                .lock()
                .unwrap()
                .values_mut()
                .find(|t| Some(&t.session) == context.as_ref())
            {
                target.dialog = None;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_dialog(
    engine: &Engine,
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
    let result = engine
        .dialog(&target.session, accept.as_bool().unwrap_or(false), deadline)
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

/// What an observation is asked for, from parameters that passed `validate`.
fn scope(params: &Value) -> page::Scope {
    page::Scope {
        filter: match params["filter"].as_str() {
            Some("interactive") => page::Filter::Interactive,
            Some("viewport") => page::Filter::Viewport,
            _ => page::Filter::All,
        },
        within: params["within"].as_str().map(str::to_owned),
        find: params["find"].as_str().map(str::to_owned),
    }
}

fn validate(operation: &str, params: &Value) -> Result<()> {
    if operation == "browser_open" {
        super::selection::parse(params)?;
    }
    let map = params
        .as_object()
        .ok_or_else(|| Failure::new("browser_invalid_params"))?;
    let allowed: &[&str] = match operation {
        "browser_open" => &["url", "profile_name", "selection"],
        "browser_sessions" | "browser_profiles" | "browser_files" => &[],
        "browser_tabs" => &["session_id"],
        "browser_screenshot" => &["session_id", "tab_id", "control_epoch"],
        "browser_observe" => &[
            "session_id",
            "tab_id",
            "control_epoch",
            "max_nodes",
            "max_chars",
            "cursor",
            "filter",
            "within",
            "find",
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
    if let Some(cursor) = params.get("cursor")
        && !cursor.as_str().is_some_and(|s| {
            s.len() == 32
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if let Some(filter) = map.get("filter")
        && !matches!(filter.as_str(), Some("all" | "interactive" | "viewport"))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if let Some(find) = map.get("find")
        && !find
            .as_str()
            .is_some_and(|s| s.chars().count() <= 128 && !s.trim().is_empty())
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    // A sequence keeps what its first call asked for.
    if map.contains_key("cursor")
        && ["filter", "within", "find"]
            .iter()
            .any(|key| map.contains_key(*key))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    for key in ["session_id", "tab_id", "within"] {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(operation: &str, effects: bool, error: Failure) -> &'static str {
        settle(operation, effects, error).state
    }

    #[test]
    fn a_refusal_by_the_site_policy_keeps_the_state_it_was_given() {
        // Refused before any request: nothing was sent, whatever else the call did.
        assert_eq!(
            state("browser_act", true, Failure::new("browser_site_denied")),
            "not_sent"
        );
        // Refused after a redirect or a response: a request has left.
        assert_eq!(
            state(
                "browser_open",
                false,
                Failure::unknown("browser_site_denied")
            ),
            "unknown"
        );
    }

    #[test]
    fn a_rejected_command_is_uncertain_only_after_an_effect() {
        let rejected = || Failure::unknown("browser_command_failed");
        for operation in [
            "browser_observe",
            "browser_screenshot",
            "browser_wait",
            "browser_act",
        ] {
            assert_eq!(
                state(operation, false, rejected()),
                "not_sent",
                "{operation}"
            );
        }
        assert_eq!(state("browser_act", true, rejected()), "unknown");
    }

    #[test]
    fn only_a_plain_product_and_version_are_reported() {
        assert_eq!(
            product("HeadlessChrome/122.0.6261.128"),
            Some(json!({"product":"HeadlessChrome","version":"122.0.6261.128"}))
        );
        assert_eq!(
            product("Chrome/157"),
            Some(json!({"product":"Chrome","version":"157"}))
        );
        for odd in [
            "",
            "Chrome",
            "/1.2",
            "Chrome/",
            "Chrome/1..2",
            "Chrome/1.2 beta",
            "My Browser/1.2",
            "Chrome/1/2",
        ] {
            assert_eq!(product(odd), None, "{odd}");
        }
        assert_eq!(product(&format!("{}/1", "a".repeat(33))), None);
    }

    #[test]
    fn a_document_still_being_parsed_is_not_reported_as_complete() {
        let observed = |state: &str| {
            let mut value = json!({"page":{"ready_state":state},"coverage":{"complete_within_scope":true,"omitted":[]}});
            mark_loading(&mut value);
            value["coverage"].clone()
        };
        assert_eq!(
            observed("loading"),
            json!({"complete_within_scope":false,"omitted":["document_loading"]})
        );
        for parsed in ["interactive", "complete"] {
            assert_eq!(
                observed(parsed),
                json!({"complete_within_scope":true,"omitted":[]}),
                "{parsed}"
            );
        }
    }

    #[test]
    fn a_navigation_is_given_up_before_the_call_deadline() {
        let now = Instant::now();
        let left = |budget: u64| {
            navigation_deadline(now, now + Duration::from_secs(budget)).duration_since(now)
        };
        assert_eq!(left(20), Duration::from_secs(17));
        assert_eq!(left(4), Duration::from_secs(3));
        assert_eq!(left(0), Duration::ZERO);
        // A deadline already behind us leaves nothing to wait for.
        assert_eq!(navigation_deadline(now + Duration::from_secs(1), now), now);
    }

    /// Checks the parameters of an observation of session `s`, with `extra` added.
    fn observation(extra: Value) -> std::result::Result<(), &'static str> {
        let mut params = json!({"session_id":"s","control_epoch":1});
        for (key, value) in extra.as_object().unwrap() {
            params[key] = value.clone();
        }
        validate("browser_observe", &params).map_err(|error| error.code)
    }

    #[test]
    fn an_observation_is_asked_for_part_of_a_page_only_where_it_starts() {
        let invalid = Err("browser_invalid_params");
        assert_eq!(observation(json!({})), Ok(()));
        for filter in ["all", "interactive", "viewport"] {
            assert_eq!(observation(json!({"filter":filter})), Ok(()), "{filter}");
        }
        assert_eq!(observation(json!({"filter":"text"})), invalid);
        assert_eq!(observation(json!({"filter":true})), invalid);
        // A continuation keeps what its sequence began with.
        let cursor = "0123456789abcdef0123456789abcdef";
        assert_eq!(observation(json!({"cursor":cursor})), Ok(()));
        assert_eq!(
            observation(json!({"cursor":cursor,"filter":"interactive"})),
            invalid
        );
        // One frame is named by its id, as an observation gives it.
        assert_eq!(
            observation(json!({"within":"66937997627883EC913DF26FAB27DE65"})),
            Ok(())
        );
        assert_eq!(observation(json!({"within":"a frame"})), invalid);
        assert_eq!(observation(json!({"within":7})), invalid);
        assert_eq!(observation(json!({"cursor":cursor,"within":"A1"})), invalid);
        assert_eq!(scope(&json!({"within":"A1"})).within.as_deref(), Some("A1"));
        assert_eq!(scope(&json!({})).within, None);
        // A text to look for has something in it, and is not long.
        assert_eq!(observation(json!({"find":"Customer 17"})), Ok(()));
        assert_eq!(observation(json!({"find":"x".repeat(128)})), Ok(()));
        assert_eq!(observation(json!({"find":"x".repeat(129)})), invalid);
        assert_eq!(observation(json!({"find":"  "})), invalid);
        assert_eq!(observation(json!({"find":17})), invalid);
        assert_eq!(observation(json!({"cursor":cursor,"find":"x"})), invalid);
        assert_eq!(scope(&json!({"find":"x"})).find.as_deref(), Some("x"));
        assert_eq!(scope(&json!({})).filter, page::Filter::All);
        assert_eq!(
            scope(&json!({"filter":"viewport"})).filter,
            page::Filter::Viewport
        );
    }

    #[test]
    fn other_failures_keep_their_meaning() {
        let refused = || Failure::new("browser_not_actionable");
        assert_eq!(state("browser_act", false, refused()), "not_sent");
        assert_eq!(state("browser_act", true, refused()), "unknown");
        for code in ["browser_timeout_unknown", "browser_lost"] {
            assert_eq!(
                state("browser_observe", false, Failure::unknown(code)),
                "unknown",
                "{code}"
            );
        }
    }
}

#[cfg(test)]
#[path = "engine/cdp/runtime_tests.rs"]
mod protocol_tests;

#[cfg(test)]
mod fake_tests;

#[cfg(all(test, target_os = "linux"))]
mod acceptance_tests;
