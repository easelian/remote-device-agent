//! Browser sessions as a peer reaches them: three operations for the AI, the
//! list of files, and the page of a signed-in person who takes over.
use super::{
    Failure, Principal, Result, Settings, Shell, address,
    control::Control,
    files::{self, Files},
    process,
    profiles::{self, Profiles},
    sites::{Lookup, Resolver, Verdict},
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
mod file_approval;
mod human;
mod v2;

const MAX_SESSIONS: usize = 2;
const MAX_TABS: usize = 8;
const IDLE: Duration = Duration::from_secs(600);

struct Admission {
    online: bool,
    closing: bool,
}
pub struct Browsers {
    v2_sessions: Arc<Mutex<HashMap<String, Arc<v2::Slot>>>>,
    settings: Settings,
    executable: Option<PathBuf>,
    root: Option<PathBuf>,
    initialized: Mutex<bool>,
    admission: Mutex<Admission>,
    profiles: Profiles,
    root_lock: Mutex<Option<Arc<crate::profile_lock::ProfileLocks>>>,
    files: Arc<Files>,
    /// Where host names lead, asked once for all sessions.
    lookup: Arc<Lookup>,
    shell: Shell,
}

impl Browsers {
    pub fn new(settings: &Settings, config_path: Option<&Path>, token: &str, shell: Shell) -> Self {
        let root = config_path.and_then(|config| super::storage_root(config, token));
        let files = Files::new(root.as_deref());
        Self {
            v2_sessions: Arc::new(Mutex::new(HashMap::new())),
            settings: settings.clone(),
            executable: process::detect(settings.executable.as_deref()),
            root,
            initialized: Mutex::new(false),
            admission: Mutex::new(Admission {
                online: true,
                closing: false,
            }),
            profiles: Profiles::default(),
            root_lock: Mutex::new(None),
            files,
            lookup: Arc::new(Lookup::default()),
            shell,
        }
    }
    /// Replaces the lookup of host names. Tests use it to say where a name leads.
    pub fn resolve_with(&self, resolver: Resolver) {
        self.lookup.resolve_with(resolver);
    }
    /// Refuses, before anything is done, an address that the site policy rules
    /// out as it is written.
    fn admit(&self, address: &Value) -> Result<()> {
        match address.as_str().map(url::Url::parse) {
            Some(Ok(address)) if self.settings.sites.judge(&address) == Verdict::Deny => {
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
        } else {
            None
        };
        let v2 = super::selection::capability(&self.settings, reason, self.executable.is_some());
        let available = v2["available"] == true;
        let mut value = json!({"version":1,"available":available,"reason":v2["reason"],"mode":"managed_headless","profiles":if self.settings.persistent_profiles {vec!["temporary","named"]}else{vec!["temporary"]},"max_sessions":MAX_SESSIONS,"max_tabs":MAX_TABS,"idle_seconds":IDLE.as_secs(),"principal_version":1,"network_isolation":false,"sites":self.settings.sites.summary(),"shell":{"enabled":self.shell.enabled,"sandboxed":self.shell.sandboxed,"coexistence":self.settings.shell_coexistence},"human_takeover":true,"screenshot":true,"file_transfer":self.settings.downloads||self.settings.uploads,"downloads":{"enabled":self.settings.downloads,"http_attachment_only":true,"declared_length_required":true,"max_file_bytes":files::MAX_FILE,"max_total_bytes":files::MAX_TOTAL,"retention_seconds":files::RETENTION},"uploads":{"enabled":self.settings.uploads,"source":"user_local_file","main_frame_only":true,"max_file_bytes":files::MAX_FILE,"grant_ttl_seconds":files::UPLOAD_TTL,"uses":1}});
        let approval_reason = if !self.settings.uploads {
            Some("browser_upload_disabled")
        } else if !available {
            Some("browser_unavailable")
        } else {
            None
        };
        value["extensions"] =
            json!({"file_authorization":super::selection::extension(approval_reason)});
        value["v2"] = v2;
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
        let runtime = tokio::runtime::Handle::current();
        match operation {
            "browser_human" => runtime.block_on(self.human(params, principal, deadline)),
            "browser_files" => {
                if !params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return Err(Failure::new("browser_invalid_params"));
                }
                self.prepare()?;
                self.files.list(principal)
            }
            // These calls check their own parameters, step by step.
            _ if v2::front::OPERATIONS.contains(&operation) => {
                runtime.block_on(self.front(operation, params, principal, deadline, operation_id))
            }
            _ => Err(Failure::new("browser_invalid_params")),
        }
    }
    pub fn stop_all(&self) {
        self.files.stop();
        let mut admission = self.admission.lock().unwrap();
        admission.closing = true;
        admission.online = false;
        self.v2_pause_all(true);
    }
    pub fn pause_all(&self) {
        let mut admission = self.admission.lock().unwrap();
        admission.online = false;
        self.v2_pause_all(false);
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
        self.v2_expire_offline();
    }
    pub async fn close(&self) -> bool {
        self.stop_all();
        let stopped = self.v2_close_all().await;
        self.files.wait().await;
        stopped
    }
    // Runtime has joined all request workers and browser-owned tasks before this call.
    pub fn release_stopped_storage(&self) {
        self.files.release_stopped_storage();
        self.root_lock.lock().unwrap().take();
    }
}

fn scoped_handle(value: &Value, prefix: &str) -> bool {
    value
        .as_str()
        .and_then(|v| v.strip_prefix(prefix))
        .is_some_and(|v| {
            v.len() == 32
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

fn validate_capture_query(value: &Value, mutation: bool) -> Result<()> {
    let invalid = || Failure::new("browser_invalid_params");
    let fields = value.as_object().ok_or_else(invalid)?;
    let id = scoped_handle(&value["capture_id"], "cap_");
    let number = |field: &str, min: u64, max: u64| {
        fields
            .get(field)
            .is_none_or(|v| v.as_u64().is_some_and(|n| (min..=max).contains(&n)))
    };
    const MAX: u64 = (1 << 53) - 1;
    let valid = match (mutation, value["kind"].as_str()) {
        (true, Some("start")) => {
            let tracking = |flag: &str, category: &str| {
                fields.get(flag).is_none_or(|v| {
                    v == true
                        && value["categories"]
                            .as_array()
                            .is_some_and(|c| c.iter().any(|v| v == category))
                })
            };
            fields.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "kind" | "categories" | "track_requests" | "track_exception_locations"
                )
            }) && value["categories"].as_array().is_some_and(|c| {
                (1..=2).contains(&c.len())
                    && c.iter()
                        .all(|v| matches!(v.as_str(), Some("network" | "console")))
                    && (c.len() == 1 || c[0] != c[1])
            }) && tracking("track_requests", "network")
                && tracking("track_exception_locations", "console")
        }
        (true, Some("stop")) => fields.len() == 2 && id,
        (false, Some("read_event" | "read_exception_location")) => {
            fields.len() == 3 && id && fields.contains_key("sequence") && number("sequence", 1, MAX)
        }
        (false, Some("read_requests" | "read_submit_outcome" | "read_submit_feedback")) => {
            fields.len() == 2 && id
        }
        (false, Some("read_submit_fields")) => {
            let indicators = value.get("include_indicators");
            let pixels = value.get("include_pixel_indicators");
            let icons = value.get("include_icon_candidates");
            id && fields.len()
                == 2 + usize::from(indicators.is_some())
                    + usize::from(pixels.is_some())
                    + usize::from(icons.is_some())
                && indicators.is_none_or(|flag| flag == true)
                && pixels.is_none_or(|flag| flag == true && value["include_indicators"] == true)
                && icons.is_none_or(|flag| {
                    flag == true
                        && value["include_pixel_indicators"] == true
                        && value["include_indicators"] == true
                })
        }
        (false, Some("read_events")) => {
            id && fields.keys().all(|k| {
                matches!(
                    k.as_str(),
                    "kind" | "capture_id" | "after_sequence" | "through_sequence" | "limit"
                )
            }) && number("after_sequence", 0, MAX)
                && number("through_sequence", 0, MAX)
                && number("limit", 1, 64)
                && value["after_sequence"].as_u64().unwrap_or(0)
                    <= value["through_sequence"].as_u64().unwrap_or(MAX)
        }
        _ => false,
    };
    if valid { Ok(()) } else { Err(invalid()) }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordCondition {
    node_id: String,
    anchor_text: String,
    expected_text: Option<String>,
}

impl RecordCondition {
    fn parse(value: &Value) -> Result<Self> {
        if !value["expected_text"].is_string() {
            return Err(Failure::new("browser_invalid_params"));
        }
        Self::parse_submit(value)
    }

    fn parse_submit(value: &Value) -> Result<Self> {
        let invalid = || Failure::new("browser_invalid_params");
        let condition: Self = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        let valid_id = condition.node_id.strip_prefix("den_").is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        let valid_text = |s: &str| {
            !s.is_empty()
                && s.trim() == s
                && s.chars().count() <= 128
                && !s.chars().any(char::is_control)
        };
        if !valid_id
            || !valid_text(&condition.anchor_text)
            || value.get("expected_text").is_some_and(Value::is_null)
            || condition
                .expected_text
                .as_deref()
                .is_some_and(|text| !valid_text(text) || condition.anchor_text == text)
        {
            return Err(invalid());
        }
        Ok(condition)
    }
}

fn ref_drag_request(operation: &str, params: &Value) -> bool {
    (operation == "browser_observe" && params["filter"] == "drag_endpoints")
        || (operation == "browser_act" && params["action"]["kind"] == "drag")
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
        "browser_tabs" => &["session_id"],
        "browser_screenshot" => &["session_id", "tab_id", "control_epoch"],
        "browser_evidence" => &["session_id", "tab_id", "control_epoch", "version", "query"],
        "browser_capture" => &["session_id", "tab_id", "control_epoch", "version", "action"],
        "browser_inspect" => &[
            "session_id",
            "tab_id",
            "control_epoch",
            "ref",
            "action_kind",
        ],
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
            "presentation",
            "format",
        ],
        "browser_act" => &[
            "session_id",
            "tab_id",
            "control_epoch",
            "action",
            "pre_action_diagnostics",
            "after_action_view",
        ],
        "browser_wait" => &[
            "session_id",
            "tab_id",
            "control_epoch",
            "url_contains",
            "text_contains",
            "record",
        ],
        "browser_control" => &["session_id", "control_epoch", "action"],
        _ => return Err(Failure::new("browser_invalid_params")),
    };
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Failure::new("browser_invalid_params"));
    }
    if map
        .get("format")
        .is_some_and(|format| format != "compact" || map.contains_key("presentation"))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if let Some(presentation) = map.get("presentation") {
        let valid = presentation.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| ["version", "mode", "base_revision"].contains(&key.as_str()))
                && object.get("version").and_then(Value::as_u64) == Some(1)
                && object.get("mode").and_then(Value::as_str) == Some("delta")
                && object.get("base_revision").is_none_or(|base| {
                    base.as_str().is_some_and(|id| {
                        !id.is_empty()
                            && id.len() <= 128
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    })
                })
        });
        if !valid || map.contains_key("cursor") {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    if matches!(operation, "browser_capture" | "browser_evidence") {
        if params["version"].as_u64() != Some(1)
            || params["control_epoch"]
                .as_u64()
                .is_none_or(|v| v == 0 || v > 9_007_199_254_740_991)
            || ["session_id", "tab_id"].iter().any(|key| {
                !params[*key].as_str().is_some_and(|v| {
                    !v.is_empty()
                        && v.len() <= 128
                        && v.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                })
            })
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if operation == "browser_capture" {
            validate_capture_query(&params["action"], true)?;
        } else if matches!(
            params["query"]["kind"].as_str(),
            Some(
                "read_events"
                    | "read_event"
                    | "read_requests"
                    | "read_exception_location"
                    | "read_submit_outcome"
                    | "read_submit_feedback"
                    | "read_submit_fields"
            )
        ) {
            validate_capture_query(&params["query"], false)?;
        } else {
            crate::browser::semantic::evidence::Query::parse(&params["query"])?;
        }
    }
    if operation == "browser_inspect"
        && (params["session_id"].as_str().is_none()
            || params["control_epoch"].as_u64().is_none()
            || !params["ref"].as_str().is_some_and(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
            || !matches!(
                params["action_kind"].as_str(),
                Some("click" | "hover" | "press" | "fill" | "check" | "select" | "attach_file")
            ))
    {
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
        && !matches!(
            filter.as_str(),
            Some("all" | "interactive" | "viewport" | "drag_endpoints" | "text")
        )
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
        ("max_nodes", 1, 10_000),
        ("max_chars", 128, 200_000),
        ("control_epoch", 1, 9_007_199_254_740_991),
    ] {
        if let Some(value) = map.get(key)
            && !value.as_u64().is_some_and(|v| v >= min && v <= max)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
    }
    // Only a compact read, or the continuation of one, takes more than the ordinary limits.
    // The read itself checks a continuation against the format it continues.
    if map.get("format").and_then(Value::as_str) != Some("compact")
        && !map.contains_key("cursor")
        && (map.get("max_nodes").and_then(Value::as_u64) > Some(1000)
            || map.get("max_chars").and_then(Value::as_u64) > Some(64000))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if let Some(url) = map.get("url")
        && !url
            .as_str()
            .is_some_and(|s| s.len() <= 4096 && address::allowed(s))
    {
        return Err(Failure::new("browser_url_denied"));
    }
    if operation == "browser_act" {
        let action = params["action"]
            .as_object()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let kind = action.get("kind").and_then(Value::as_str).unwrap_or("");
        if let Some(view) = params.get("after_action_view")
            && (view != true
                || !matches!(
                    kind,
                    "click"
                        | "check"
                        | "select"
                        | "fill"
                        | "press"
                        | "hover"
                        | "scroll"
                        | "drag"
                        | "navigate"
                        | "visual_click"
                        | "visual_hover"
                        | "visual_drag"
                ))
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if let Some(options) = params.get("pre_action_diagnostics") {
            if kind != "click"
                || !map.contains_key("tab_id")
                || !map.contains_key("control_epoch")
                || action
                    .get("ref")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                || !options.is_object()
                || options.get("kind").is_some()
            {
                return Err(Failure::new("browser_invalid_params"));
            }
            let mut start = options.clone();
            if let Some(record) = start
                .as_object_mut()
                .and_then(|options| options.remove("rejection_record"))
            {
                if start["track_requests"] != true {
                    return Err(Failure::new("browser_invalid_params"));
                }
                RecordCondition::parse_submit(&record)?;
                if let Some(flag) = start
                    .as_object_mut()
                    .unwrap()
                    .remove("capture_field_pixels")
                    && flag != true
                {
                    return Err(Failure::new("browser_invalid_params"));
                }
            }
            start["kind"] = "start".into();
            validate_capture_query(&start, true)?;
        }
        let fields: &[&str] = match kind {
            "navigate" => &["kind", "url"],
            "fill" => &["kind", "ref", "text"],
            "press"
                if action.get("read_validation_field").is_some() && action["key"] == "Enter" =>
            {
                &[
                    "kind",
                    "ref",
                    "key",
                    "read_native_validation",
                    "read_validation_field",
                ]
            }
            "click" if action.get("read_validation_field").is_some() => &[
                "kind",
                "ref",
                "read_native_validation",
                "read_validation_field",
            ],
            "press"
                if action.get("read_native_validation").is_some() && action["key"] == "Enter" =>
            {
                &["kind", "ref", "key", "read_native_validation"]
            }
            "press" => &["kind", "ref", "key"],
            "check" => &["kind", "ref", "checked"],
            "select" => &["kind", "ref", "value"],
            "attach_file" => &["kind", "ref", "file_grant_id"],
            "scroll" => {
                if action.len() == 3
                    && action.get("ref").and_then(Value::as_str).is_some()
                    && action.get("delta_y").is_some()
                {
                    &["kind", "ref", "delta_y"]
                } else {
                    &["kind", "delta_y"]
                }
            }
            "click" if action.get("read_native_validation").is_some() => {
                &["kind", "ref", "read_native_validation"]
            }
            "click" if action.get("manner").is_some() => {
                if !matches!(action["manner"].as_str(), Some("double" | "right")) {
                    return Err(Failure::new("browser_invalid_params"));
                }
                &["kind", "ref", "manner"]
            }
            "click" => &["kind", "ref"],
            "hover" => &["kind", "ref"],
            "hover_submit_icon" => &["kind", "capture_id", "field_node_id", "source_node_id"],
            "visual_click" | "visual_hover" => &["kind", "frame_id", "x", "y"],
            "visual_drag" => &["kind", "frame_id", "x", "y", "to_x", "to_y"],
            "drag" => &["kind", "ref", "target_ref"],
            "new_tab" | "close_tab" | "select_tab" => &["kind"],
            "dialog" => &["kind", "accept", "dialog_id"],
            _ => return Err(Failure::new("browser_invalid_params")),
        };
        if action
            .get("read_validation_field")
            .is_some_and(|v| v != true)
            || action.len() != fields.len()
            || action.keys().any(|key| !fields.contains(&key.as_str()))
            || action
                .get("read_native_validation")
                .is_some_and(|value| value != true)
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if matches!(kind, "visual_click" | "visual_hover" | "visual_drag") {
            let valid_frame = action["frame_id"]
                .as_str()
                .and_then(|id| id.strip_prefix("vfrm_"))
                .is_some_and(|hex| {
                    hex.len() == 32
                        && hex
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                });
            if !valid_frame
                || fields[2..]
                    .iter()
                    .any(|&key| action[key].as_u64().is_none_or(|n| n >= 1920))
                || params["tab_id"].as_str().is_none_or(str::is_empty)
                || params["control_epoch"]
                    .as_u64()
                    .is_none_or(|epoch| epoch == 0)
            {
                return Err(Failure::new("browser_invalid_params"));
            }
        }
        if kind == "navigate"
            && !action["url"]
                .as_str()
                .is_some_and(|s| s.len() <= 4096 && address::allowed(s))
        {
            return Err(Failure::new("browser_url_denied"));
        }
        if kind == "hover_submit_icon"
            && (!scoped_handle(&action["capture_id"], "cap_")
                || !scoped_handle(&action["field_node_id"], "den_")
                || !scoped_handle(&action["source_node_id"], "den_")
                || action["field_node_id"] == action["source_node_id"]
                || params["tab_id"].as_str().is_none_or(str::is_empty)
                || params["control_epoch"]
                    .as_u64()
                    .is_none_or(|epoch| epoch == 0))
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if fields.contains(&"ref") && action["ref"].as_str().is_none_or(|s| s.len() > 128) {
            return Err(Failure::new("browser_invalid_params"));
        }
        if kind == "drag"
            && ["ref", "target_ref"].iter().any(|key| {
                !action[*key].as_str().is_some_and(|id| {
                    !id.is_empty()
                        && id.len() <= 128
                        && id
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
                })
            })
        {
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
                && action.get("delta_y").is_some_and(|value| {
                    value
                        .as_f64()
                        .is_none_or(|n| !n.is_finite() || n.abs() > 100_000.)
                })
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
        && !matches!(
            params["action"].as_str(),
            Some("pause" | "resume" | "close")
        )
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    if operation == "browser_wait" {
        let count = ["url_contains", "text_contains", "record"]
            .iter()
            .filter(|key| map.contains_key(**key))
            .count();
        if count != 1 {
            return Err(Failure::new("browser_invalid_params"));
        }
        if let Some(record) = map.get("record") {
            if !map.contains_key("tab_id") {
                return Err(Failure::new("browser_invalid_params"));
            }
            RecordCondition::parse(record)?;
        } else if !map
            .get("url_contains")
            .or_else(|| map.get("text_contains"))
            .and_then(Value::as_str)
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

    #[test]
    fn task_event_capture_validation_is_closed_and_versioned() {
        let base = json!({"version":1,"session_id":"s","tab_id":"t","control_epoch":1,"action":{"kind":"start","categories":["network"]}});
        assert!(validate("browser_capture", &base).is_ok());
        let mut tracked = base.clone();
        tracked["action"]["track_requests"] = true.into();
        assert!(validate("browser_capture", &tracked).is_ok());
        let mut unselected = base.clone();
        unselected["action"]["categories"] = json!(["console"]);
        unselected["action"]["track_requests"] = true.into();
        assert!(validate("browser_capture", &unselected).is_err());
        let mut both = tracked.clone();
        both["action"]["track_exception_locations"] = true.into();
        assert!(validate("browser_capture", &both).is_err());
        both["action"]["categories"] = json!(["network", "console"]);
        assert!(validate("browser_capture", &both).is_ok());
        both["action"]["track_exception_locations"] = false.into();
        assert!(validate("browser_capture", &both).is_err());
        let mut extra = base.clone();
        extra["action"]["extra"] = true.into();
        assert!(validate("browser_capture", &extra).is_err());
        let stop = json!({"version":1,"session_id":"s","tab_id":"t","control_epoch":1,"action":{"kind":"stop","capture_id":format!("cap_{}", "a".repeat(32))}});
        assert!(validate("browser_capture", &stop).is_ok());
        let read = json!({"version":1,"session_id":"s","tab_id":"t","control_epoch":1,"query":{"kind":"read_events","capture_id":format!("cap_{}", "a".repeat(32))}});
        assert!(validate("browser_evidence", &read).is_ok());
        let requests = json!({"version":1,"session_id":"s","tab_id":"t","control_epoch":1,"query":{"kind":"read_requests","capture_id":format!("cap_{}", "a".repeat(32))}});
        assert!(validate("browser_evidence", &requests).is_ok());
        let mut bad = read.clone();
        bad["query"]["through_sequence"] = 0.into();
        assert!(validate("browser_evidence", &bad).is_ok());
        bad["query"]["limit"] = false.into();
        assert!(validate("browser_evidence", &bad).is_err());
        let mut old = read.clone();
        old["version"] = 2.into();
        assert!(validate("browser_evidence", &old).is_err());
    }

    #[test]
    fn submit_fields_validator_scopes_indicator_opt_in() {
        let cap = format!("cap_{}", "a".repeat(32));
        let base = json!({
            "version":1,"session_id":"s","tab_id":"t","control_epoch":1,
            "query":{"kind":"read_submit_fields","capture_id":cap}
        });
        assert!(validate("browser_evidence", &base).is_ok());
        let mut enabled = base.clone();
        enabled["query"]["include_indicators"] = true.into();
        assert!(validate("browser_evidence", &enabled).is_ok());
        enabled["query"]["include_pixel_indicators"] = true.into();
        assert!(validate("browser_evidence", &enabled).is_ok());
        enabled["query"]["include_icon_candidates"] = true.into();
        assert!(validate("browser_evidence", &enabled).is_ok());
        enabled["query"]
            .as_object_mut()
            .unwrap()
            .remove("include_indicators");
        assert!(validate("browser_evidence", &enabled).is_err());
        let mut icon_without_pixels = base.clone();
        icon_without_pixels["query"]["include_indicators"] = true.into();
        icon_without_pixels["query"]["include_icon_candidates"] = true.into();
        assert!(validate("browser_evidence", &icon_without_pixels).is_err());
        let mut disabled = base.clone();
        disabled["query"]["include_indicators"] = false.into();
        assert!(validate("browser_evidence", &disabled).is_err());
        let mut legacy = json!({
            "version":1,"session_id":"s","tab_id":"t","control_epoch":1,
            "query":{"kind":"read_submit_feedback","capture_id":cap}
        });
        legacy["query"]["include_indicators"] = true.into();
        assert!(validate("browser_evidence", &legacy).is_err());
        let mut extra = base;
        extra["query"]["unexpected"] = true.into();
        assert!(validate("browser_evidence", &extra).is_err());
    }

    #[test]
    fn task_event_capture_rejects_non_hex_capture_ids() {
        let request = json!({"version":1,"session_id":"s","tab_id":"t","control_epoch":1,"action":{"kind":"stop","capture_id":format!("cap_{}", "g".repeat(32))}});
        assert!(validate("browser_capture", &request).is_err());
    }

    #[test]
    fn delta_presentation_is_explicit_closed_and_incompatible_with_a_cursor() {
        let plain = json!({"session_id":"session","control_epoch":1});
        assert!(validate("browser_observe", &plain).is_ok());
        let valid = json!({"version":1,"mode":"delta","base_revision":"r".repeat(128)});
        let mut request = plain.clone();
        request["presentation"] = valid.clone();
        assert!(validate("browser_observe", &request).is_ok());
        request["presentation"]
            .as_object_mut()
            .unwrap()
            .remove("base_revision");
        assert!(validate("browser_observe", &request).is_ok());
        for presentation in [
            Value::Null,
            json!({}),
            json!({"version":2,"mode":"delta"}),
            json!({"version":1,"mode":"full"}),
            json!({"version":1,"mode":"delta","extra":true}),
            json!({"version":1,"mode":"delta","base_revision":null}),
            json!({"version":1,"mode":"delta","base_revision":""}),
            json!({"version":1,"mode":"delta","base_revision":"r".repeat(129)}),
            json!({"version":1,"mode":"delta","base_revision":"é"}),
        ] {
            request["presentation"] = presentation;
            assert_eq!(
                validate("browser_observe", &request).unwrap_err().code,
                "browser_invalid_params"
            );
        }
        request["presentation"] = valid;
        request["cursor"] = "0123456789abcdef0123456789abcdef".into();
        assert_eq!(
            validate("browser_observe", &request).unwrap_err().code,
            "browser_invalid_params"
        );
    }

    #[test]
    fn diagnostics_accept_only_a_complete_bounded_read_request() {
        let valid =
            json!({"session_id":"session","control_epoch":1,"ref":"r","action_kind":"click"});
        assert!(validate("browser_inspect", &valid).is_ok());
        for kind in [
            "click",
            "hover",
            "press",
            "fill",
            "check",
            "select",
            "attach_file",
        ] {
            let mut request = valid.clone();
            request["action_kind"] = kind.into();
            request["tab_id"] = "tab".into();
            request["ref"] = "r".repeat(128).into();
            assert!(validate("browser_inspect", &request).is_ok());
        }
        for required in ["session_id", "control_epoch", "ref", "action_kind"] {
            let mut request = valid.clone();
            request.as_object_mut().unwrap().remove(required);
            assert_eq!(
                validate("browser_inspect", &request).unwrap_err().code,
                "browser_invalid_params"
            );
        }
        for (key, value) in [
            ("ref", json!("")),
            ("ref", json!("r".repeat(129))),
            ("ref", json!("é")),
            ("ref", json!(false)),
            ("action_kind", json!("navigate")),
            ("action_kind", Value::Null),
            ("session_id", json!("")),
            ("tab_id", Value::Null),
            ("control_epoch", json!(0)),
            ("control_epoch", json!(9_007_199_254_740_992u64)),
            ("control_epoch", json!(1.5)),
            ("operation_id", json!("not-a-mutation")),
            ("text", json!("not-an-input")),
            ("selector", json!("button")),
            ("file_grant_id", json!("not-consumed")),
            ("presentation", json!({})),
        ] {
            let mut request = valid.clone();
            request[key] = value;
            assert_eq!(
                validate("browser_inspect", &request).unwrap_err().code,
                "browser_invalid_params",
                "{request}"
            );
        }
    }

    #[test]
    fn scroll_accepts_reference_or_document_delta_but_no_mixed_form() {
        let base = json!({"session_id":"session","control_epoch":1,"action":{"kind":"scroll","ref":"r","delta_y":300}});
        assert!(validate("browser_act", &base).is_ok());
        let delta = json!({"session_id":"session","control_epoch":1,"action":{"kind":"scroll","delta_y":300}});
        assert!(validate("browser_act", &delta).is_ok());
        let mixed = json!({"session_id":"session","control_epoch":1,"action":{"kind":"scroll","ref":"r","delta_y":300,"extra":true}});
        assert_eq!(
            validate("browser_act", &mixed).unwrap_err().code,
            "browser_invalid_params"
        );
    }

    #[test]
    fn pre_action_diagnostics_is_limited_to_ref_click() {
        let base = json!({"session_id":"session","tab_id":"tab","control_epoch":1,"action":{"kind":"click","ref":"r"},"pre_action_diagnostics":{"categories":["console"]}});
        assert!(validate("browser_act", &base).is_ok());
        let mut bad = base.clone();
        bad["pre_action_diagnostics"]["extra"] = true.into();
        assert!(validate("browser_act", &bad).is_err());
        let mut bad = base.clone();
        bad["pre_action_diagnostics"]["categories"] = json!(["dom"]);
        assert!(validate("browser_act", &bad).is_err());
        for kind in ["fill", "navigate", "hover"] {
            let mut bad = base.clone();
            bad["action"]["kind"] = kind.into();
            assert!(validate("browser_act", &bad).is_err());
        }
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
        for filter in ["all", "interactive", "viewport", "text"] {
            assert_eq!(observation(json!({"filter":filter})), Ok(()), "{filter}");
        }
        assert_eq!(observation(json!({"filter":"words"})), invalid);
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
        // A text to look for has something in it, and is not long.
        assert_eq!(observation(json!({"find":"Customer 17"})), Ok(()));
        assert_eq!(observation(json!({"find":"x".repeat(128)})), Ok(()));
        assert_eq!(observation(json!({"find":"x".repeat(129)})), invalid);
        assert_eq!(observation(json!({"find":"  "})), invalid);
        assert_eq!(observation(json!({"find":17})), invalid);
        assert_eq!(observation(json!({"cursor":cursor,"find":"x"})), invalid);
    }
}
