//! Human-approved copies share the artifact budget; only one receipted selection is allowed.
use super::*;

pub const UPLOAD_TTL: u64 = 600;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Upload {
    human_owner: String,
    lease: String,
    received: u64,
    operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval: Option<Approval>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    version: u32,
    view: String,
    token_hash: String,
    epoch: u64,
    // Publication is valid only in this Agent instance. On disk the record is
    // hidden until consumption; every upload is discarded during recovery.
    #[serde(skip)]
    published: bool,
}
impl Upload {
    pub(super) fn disclosable(&self) -> bool {
        self.approval.as_ref().is_none_or(|a| a.published)
    }
    pub(super) fn format_matches(&self, version: u32) -> bool {
        match &self.approval {
            None => version == 1,
            Some(a) => {
                version == 2
                    && a.version == 1
                    && id_valid(&a.view)
                    && hash_valid(&a.token_hash)
                    && a.epoch > 0
                    && self.lease.is_empty()
            }
        }
    }
    fn matches_approval(&self, human: &Principal, params: &Value, with_epoch: bool) -> bool {
        human.channel == "human"
            && self.human_owner == human.owner
            && self.approval.as_ref().is_some_and(|a| {
                params["view_id"] == a.view
                    && params["approval_token"]
                        .as_str()
                        .filter(|s| id_valid(s))
                        .is_some_and(|token| {
                            crate::config::sha256_hex(token.as_bytes()) == a.token_hash
                        })
                    && (!with_epoch || params["control_epoch"].as_u64() == Some(a.epoch))
            })
    }
}

pub(super) fn value(meta: &Metadata) -> Value {
    json!({"file_grant_id":meta.id,"session_id":meta.session_id,"tab_id":meta.tab_id,
        "name":meta.name,"source_origin":meta.origin,"size_bytes":meta.size_bytes,
        "sha256":meta.sha256,"state":meta.state,"expires_at":meta.expires_at,
        "next_offset":meta.upload.as_ref().map(|u|u.received),"uses":1})
}

impl Files {
    pub fn upload_begin(
        &self,
        owner: &Owner,
        human: &Principal,
        lease: &str,
        params: &Value,
    ) -> Result<Value> {
        self.begin_upload(
            owner,
            human,
            Upload {
                human_owner: human.owner.clone(),
                lease: lease.into(),
                received: 0,
                operation: None,
                approval: None,
            },
            params,
        )
    }

    pub fn approval_begin(
        &self,
        owner: &Owner,
        human: &Principal,
        params: &Value,
    ) -> Result<Value> {
        let view = params["view_id"]
            .as_str()
            .filter(|s| id_valid(s))
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let epoch = params["control_epoch"]
            .as_u64()
            .filter(|v| *v > 0)
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let token = crate::random::hex_token::<16>().map_err(io_error)?;
        let upload = Upload {
            human_owner: human.owner.clone(),
            lease: String::new(),
            received: 0,
            operation: None,
            approval: Some(Approval {
                version: 1,
                view: view.into(),
                token_hash: crate::config::sha256_hex(token.as_bytes()),
                epoch,
                published: false,
            }),
        };
        let mut result = self.begin_upload(owner, human, upload, params)?;
        result["approval_token"] = token.into();
        result["available_to_ai"] = false.into();
        Ok(result)
    }

    fn begin_upload(
        &self,
        owner: &Owner,
        human: &Principal,
        upload: Upload,
        params: &Value,
    ) -> Result<Value> {
        let name = params["name"]
            .as_str()
            .filter(|name| {
                !name.trim().is_empty()
                    && !matches!(*name, "." | "..")
                    && name.len() <= 240
                    && !name
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
            })
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let size = params["size_bytes"]
            .as_u64()
            .filter(|s| *s <= MAX_FILE)
            .ok_or_else(|| Failure::new("browser_file_limit"))?;
        let sha256 = params["sha256"]
            .as_str()
            .filter(|h| hash_valid(h))
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        if params["approved"] != true
            || human.channel != "human"
            || human.user != owner.principal.user
            || params["source_origin"] != owner.origin
        {
            return Err(Failure::new("browser_upload_denied"));
        }
        self.expire()?;
        let mut catalog = self.catalog.lock().unwrap();
        if self.stopping.load(Ordering::Acquire)
            || catalog.entries.len() >= MAX_FILES
            || catalog
                .entries
                .values()
                .map(|e| e.meta.size_bytes)
                .sum::<u64>()
                + size
                > MAX_TOTAL
        {
            return Err(Failure::new("browser_file_limit"));
        }
        let created = now();
        let meta = Metadata {
            cancellation_scope: None,
            version: if upload.approval.is_some() { 2 } else { 1 },
            id: crate::random::hex_token::<16>().map_err(io_error)?,
            owner: owner.principal.owner.clone(),
            user: owner.principal.user.clone(),
            session_id: owner.session.clone(),
            tab_id: owner.tab.clone(),
            origin: owner.origin.clone(),
            name: name.into(),
            size_bytes: size,
            sha256: sha256.into(),
            state: "staging".into(),
            created_at: created,
            expires_at: created + UPLOAD_TTL,
            ai_visible: false,
            upload: Some(upload),
        };
        let path = self.path(&meta.id)?;
        crate::storage::create_private_dir(&path).map_err(io_error)?;
        // Retain the reservation if any cleanup fails.
        catalog.entries.insert(
            meta.id.clone(),
            Entry {
                meta: meta.clone(),
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let result = (|| {
            self.save(&meta)?;
            crate::storage::create_private_file(&path.join("content")).map_err(io_error)?;
            let mut result = value(&meta);
            result["offset"] = 0.into();
            Ok(result)
        })();
        if result.is_err() && fs::remove_dir_all(&path).is_ok() {
            catalog.entries.remove(&meta.id);
        }
        result
    }

    pub fn upload_step(
        &self,
        human: &Principal,
        session: &str,
        lease: &str,
        params: &Value,
    ) -> Result<Value> {
        self.step_upload(human, session, Some(lease), params, None)
    }

    /// Validate bytes and persist the approved-but-hidden record before the
    /// runtime takes the final control fence. Only publish_approval can reveal it.
    pub fn approval_step(
        &self,
        human: &Principal,
        params: &Value,
        owner: Option<&Owner>,
    ) -> Result<Value> {
        let session = params["session_id"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        self.step_upload(human, session, None, params, owner)
    }

    fn step_upload(
        &self,
        human: &Principal,
        session: &str,
        lease: Option<&str>,
        params: &Value,
        owner: Option<&Owner>,
    ) -> Result<Value> {
        let action = params["action"].as_str().unwrap_or("");
        let action = if lease.is_some() {
            action
        } else {
            action.strip_prefix("file_").unwrap_or("")
        };
        let id = params["file_grant_id"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_invalid_params"))?;
        let path = self.path(id)?;
        let mut catalog = self.catalog.lock().unwrap();
        let entry = catalog
            .entries
            .get_mut(id)
            .filter(|e| {
                e.meta.user == human.user
                    && e.meta.session_id == session
                    && e.meta.expires_at > now()
                    && e.meta.upload.as_ref().is_some_and(|u| match lease {
                        Some(lease) => {
                            human.channel == "human"
                                && u.approval.is_none()
                                && u.human_owner == human.owner
                                && u.lease == lease
                                && u.operation.is_none()
                        }
                        None => {
                            u.matches_approval(
                                human,
                                params,
                                !matches!(action, "upload_status" | "upload_cancel"),
                            ) && (action == "upload_status" || u.operation.is_none())
                        }
                    })
            })
            .ok_or_else(|| Failure::new("browser_upload_denied"))?;
        if action == "upload_status" && lease.is_none() {
            let mut result = value(&entry.meta);
            result["available_to_ai"] = entry.meta.ai_visible.into();
            return Ok(result);
        }
        if action == "upload_cancel" {
            fs::remove_dir_all(&path).map_err(|_| Failure::unknown("browser_io_error"))?;
            catalog.entries.remove(id);
            return Ok(json!({"file_grant_id":id,"file_deleted":true}));
        }
        if entry.meta.state != "staging" {
            return Err(Failure::new("browser_upload_denied"));
        }
        let upload = entry.meta.upload.as_mut().unwrap();
        if action == "upload_chunk" {
            let encoded = params["data"]
                .as_str()
                .filter(|s| s.len() <= CHUNK.div_ceil(3) * 4)
                .ok_or_else(|| Failure::new("browser_file_limit"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Failure::new("browser_invalid_params"))?;
            if bytes.is_empty()
                || bytes.len() > CHUNK
                || params["offset"].as_u64() != Some(upload.received)
                || upload.received + bytes.len() as u64 > entry.meta.size_bytes
            {
                return Err(Failure::new("browser_upload_offset"));
            }
            let mut file =
                crate::storage::open_private_file(&path.join("content"), true).map_err(io_error)?;
            let stat = file.metadata().map_err(io_error)?;
            if stat.len() != upload.received {
                return Err(Failure::new("browser_file_changed"));
            }
            file.seek(SeekFrom::End(0)).map_err(io_error)?;
            file.write_all(&bytes)
                .map_err(|_| Failure::unknown("browser_io_error"))?;
            upload.received += bytes.len() as u64;
            // Staging progress is intentionally instance-local. Restart discards it.
            return Ok(value(&entry.meta));
        }
        if action != "upload_finish" || upload.received != entry.meta.size_bytes {
            return Err(Failure::new("browser_file_incomplete"));
        }
        if lease.is_none()
            && !owner.is_some_and(|owner| {
                entry.meta.owner == owner.principal.owner
                    && entry.meta.user == owner.principal.user
                    && entry.meta.session_id == owner.session
                    && entry.meta.tab_id == owner.tab
                    && entry.meta.origin == owner.origin
            })
        {
            return Err(Failure::new("browser_upload_denied"));
        }
        let mut file =
            crate::storage::open_private_file(&path.join("content"), true).map_err(io_error)?;
        if file.metadata().map_err(io_error)?.len() != entry.meta.size_bytes
            || hash_file(&mut file)? != entry.meta.sha256
        {
            return Err(Failure::new("browser_file_changed"));
        }
        file.sync_all().map_err(io_error)?;
        // Windows does not permit renaming an open stream that denies deletion.
        drop(file);
        let payload = path.join("payload");
        crate::storage::create_private_dir(&payload).map_err(io_error)?;
        fs::rename(path.join("content"), payload.join(&entry.meta.name)).map_err(io_error)?;
        let mut complete = entry.meta.clone();
        complete.state = "approved".into();
        self.save(&complete)?;
        entry.meta = complete;
        Ok(value(&entry.meta))
    }

    /// The caller holds the current control fence. This publication does no I/O:
    /// upload records never survive an Agent instance, and consumption is still
    /// persisted before any native selection can be enqueued.
    pub fn publish_approval(
        &self,
        owner: &Owner,
        human: &Principal,
        params: &Value,
    ) -> Result<Value> {
        let mut catalog = self
            .catalog
            .try_lock()
            .map_err(|_| Failure::unknown("browser_busy"))?;
        let entry = params["file_grant_id"]
            .as_str()
            .and_then(|id| catalog.entries.get_mut(id))
            .filter(|e| {
                e.meta.version == 2
                    && e.meta.state == "approved"
                    && !e.meta.ai_visible
                    && e.meta.owner == owner.principal.owner
                    && e.meta.user == owner.principal.user
                    && e.meta.user == human.user
                    && e.meta.session_id == owner.session
                    && e.meta.tab_id == owner.tab
                    && e.meta.origin == owner.origin
                    && e.meta.expires_at > now()
                    && !self.stopping.load(Ordering::Acquire)
                    && e.meta.upload.as_ref().is_some_and(|u| {
                        u.operation.is_none() && u.matches_approval(human, params, true)
                    })
            })
            .ok_or_else(|| Failure::new("browser_upload_denied"))?;
        entry
            .meta
            .upload
            .as_mut()
            .unwrap()
            .approval
            .as_mut()
            .unwrap()
            .published = true;
        entry.meta.ai_visible = true;
        let mut result = value(&entry.meta);
        result["available_to_ai"] = true.into();
        Ok(result)
    }

    pub fn consume_upload(&self, owner: &Owner, id: &str, operation: &str) -> Result<PathBuf> {
        let mut catalog = self.catalog.lock().unwrap();
        let entry = catalog
            .entries
            .get_mut(id)
            .filter(|e| {
                e.meta.allowed(&owner.principal)
                    && e.meta.owner == owner.principal.owner
                    && e.meta.session_id == owner.session
                    && e.meta.tab_id == owner.tab
                    && e.meta.origin == owner.origin
                    && e.meta.expires_at > now()
                    && e.meta.state == "approved"
                    && e.meta
                        .upload
                        .as_ref()
                        .is_some_and(|u| u.operation.is_none())
            })
            .ok_or_else(|| Failure::new("browser_upload_denied"))?;
        let path = self.path(id)?.join("payload").join(&entry.meta.name);
        let mut content = open_content(&path)?;
        if content.metadata().map_err(io_error)?.len() != entry.meta.size_bytes
            || hash_file(&mut content)? != entry.meta.sha256
        {
            return Err(Failure::new("browser_file_changed"));
        }
        // Persist consumption before enqueue. Even an unknown CDP result never restores it.
        let mut consumed = entry.meta.clone();
        consumed.state = "consumed".into();
        consumed.upload.as_mut().unwrap().operation = Some(operation.into());
        consumed.expires_at = consumed.created_at + RETENTION;
        self.save(&consumed)?;
        entry.meta = consumed;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        files: Arc<Files>,
        owner: Owner,
        human: Principal,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "rdh-upload-unit-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            crate::storage::create_private_dir(&root).unwrap();
            let files = Files::new(Some(&root));
            files.prepare().unwrap();
            let principal = Principal {
                version: 1,
                user: "a".repeat(64),
                owner: "b".repeat(64),
                channel: "ai".into(),
            };
            let human = Principal {
                owner: "c".repeat(64),
                channel: "human".into(),
                ..principal.clone()
            };
            let owner = Owner {
                principal,
                session: "session".into(),
                tab: "tab".into(),
                origin: "https://example.test".into(),
                control: Arc::new(Control::new()),
            };
            Self {
                root,
                files,
                owner,
                human,
            }
        }
        fn begin(&self, size: u64, sha: &str) -> Result<Value> {
            self.files.upload_begin(&self.owner,&self.human,"lease",&json!({"name":"proof.bin","size_bytes":size,"sha256":sha,"approved":true,"source_origin":self.owner.origin}))
        }
        fn step(&self, action: &str, id: &str, extra: Value) -> Result<Value> {
            let mut params = json!({"action":action,"file_grant_id":id});
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            self.files
                .upload_step(&self.human, "session", "lease", &params)
        }
    }
    #[test]
    fn file_replacement_expiry_and_consumer_binding_preserve_one_use() {
        let f = Fixture::new();
        let bytes = b"approved content";
        let sha = crate::config::sha256_hex(bytes);
        let begin = f.begin(bytes.len() as u64, &sha).unwrap();
        let id = begin["file_grant_id"].as_str().unwrap();
        f.step(
            "upload_chunk",
            id,
            json!({"offset":0,"data":base64::engine::general_purpose::STANDARD.encode(bytes)}),
        )
        .unwrap();
        f.step("upload_finish", id, json!({})).unwrap();
        assert_eq!(
            f.files.list(&f.owner.principal).unwrap()["uploads"],
            json!([])
        );
        f.files.disclosure("session", true).unwrap();
        let mut wrong = f.owner.clone();
        wrong.origin = "https://wrong.test".into();
        assert_eq!(
            f.files.consume_upload(&wrong, id, "op").unwrap_err().code,
            "browser_upload_denied"
        );
        wrong = f.owner.clone();
        wrong.tab = "other".into();
        assert_eq!(
            f.files.consume_upload(&wrong, id, "op").unwrap_err().code,
            "browser_upload_denied"
        );
        wrong = f.owner.clone();
        wrong.principal.owner = "d".repeat(64);
        assert_eq!(
            f.files.consume_upload(&wrong, id, "op").unwrap_err().code,
            "browser_upload_denied"
        );
        let path = f.files.path(id).unwrap().join("payload/proof.bin");
        fs::write(&path, b"replaced content").unwrap();
        assert_eq!(
            f.files.consume_upload(&f.owner, id, "op").unwrap_err().code,
            "browser_file_changed"
        );
        fs::write(&path, bytes).unwrap();
        {
            let mut c = f.files.catalog.lock().unwrap();
            c.entries.get_mut(id).unwrap().meta.expires_at = now() - 1;
        }
        assert_eq!(
            f.files.consume_upload(&f.owner, id, "op").unwrap_err().code,
            "browser_upload_denied"
        );
        {
            let mut c = f.files.catalog.lock().unwrap();
            c.entries.get_mut(id).unwrap().meta.expires_at = now() + 60;
        }
        assert_eq!(f.files.consume_upload(&f.owner, id, "op").unwrap(), path);
        for op in ["op", "different-op"] {
            assert_eq!(
                f.files.consume_upload(&f.owner, id, op).unwrap_err().code,
                "browser_upload_denied"
            );
        }
        assert_eq!(
            f.files.read(&f.human, id, 0).unwrap_err().code,
            "browser_file_unavailable"
        );
        // A restarted instance cannot recover a grant, including a consumed grant.
        let restarted = Files::new(Some(&f.root));
        restarted.prepare().unwrap();
        assert_eq!(restarted.list(&f.human).unwrap()["uploads"], json!([]));
        assert!(!path.exists());
    }
    #[test]
    fn staging_reservations_share_the_limit_and_cancel_releases_space() {
        let f = Fixture::new();
        let sha = "0".repeat(64);
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(
                f.begin(MAX_FILE, &sha).unwrap()["file_grant_id"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert_eq!(f.begin(1, &sha).unwrap_err().code, "browser_file_limit");
        f.step("upload_cancel", &ids[0], json!({})).unwrap();
        assert!(f.begin(MAX_FILE, &sha).is_ok());
        let mut c = f.files.catalog.lock().unwrap();
        c.entries.get_mut(&ids[1]).unwrap().meta.expires_at = now() - 1;
        drop(c);
        f.files.expire().unwrap();
        assert!(!f.files.path(&ids[1]).unwrap().exists());
        assert!(f.begin(MAX_FILE, &sha).is_ok());
    }

    impl Fixture {
        fn independent(&self, bytes: &[u8]) -> Value {
            self.files
                .approval_begin(
                    &self.owner,
                    &self.human,
                    &json!({
                        "view_id":"e".repeat(32),"control_epoch":1,"approved":true,
                        "name":"approved.bin","size_bytes":bytes.len(),
                        "sha256":crate::config::sha256_hex(bytes),"source_origin":self.owner.origin,
                    }),
                )
                .unwrap()
        }
        fn approval_params(&self, grant: &Value, action: &str) -> Value {
            json!({"action":action,"session_id":self.owner.session,"view_id":"e".repeat(32),
                "file_grant_id":grant["file_grant_id"],"approval_token":grant["approval_token"],
                "control_epoch":1})
        }
        fn stage(&self, grant: &Value, bytes: &[u8]) -> Value {
            let mut chunk = self.approval_params(grant, "file_upload_chunk");
            chunk["offset"] = 0.into();
            chunk["data"] = base64::engine::general_purpose::STANDARD
                .encode(bytes)
                .into();
            self.files.approval_step(&self.human, &chunk, None).unwrap();
            let finish = self.approval_params(grant, "file_upload_finish");
            self.files
                .approval_step(&self.human, &finish, Some(&self.owner))
                .unwrap();
            finish
        }
    }
    #[test]
    fn independent_approval_authenticates_each_identity_and_recovers_original_offset() {
        let f = Fixture::new();
        let bytes = b"user approved payload";
        let grant = f.independent(bytes);
        let id = grant["file_grant_id"].as_str().unwrap();
        let raw = fs::read_to_string(f.files.path(id).unwrap().join("metadata.json")).unwrap();
        assert!(!raw.contains(grant["approval_token"].as_str().unwrap()));
        let disk: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(disk["version"], 2);
        assert_eq!(
            disk["upload"]["approval"]["token_hash"],
            crate::config::sha256_hex(grant["approval_token"].as_str().unwrap().as_bytes())
        );
        let mut chunk = f.approval_params(&grant, "file_upload_chunk");
        chunk["offset"] = 0.into();
        chunk["data"] = base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .into();
        for (key, value) in [
            ("view_id", json!("f".repeat(32))),
            ("approval_token", json!("0".repeat(32))),
            ("control_epoch", json!(2)),
            ("session_id", json!("another-session")),
        ] {
            let mut wrong = chunk.clone();
            wrong[key] = value;
            assert_eq!(
                f.files
                    .approval_step(&f.human, &wrong, None)
                    .unwrap_err()
                    .code,
                "browser_upload_denied"
            );
        }
        for field in ["user", "owner", "channel"] {
            let mut human = f.human.clone();
            match field {
                "user" => human.user = "d".repeat(64),
                "owner" => human.owner = "d".repeat(64),
                _ => human.channel = "ai".into(),
            };
            assert_eq!(
                f.files
                    .approval_step(&human, &chunk, None)
                    .unwrap_err()
                    .code,
                "browser_upload_denied"
            );
        }
        f.files.approval_step(&f.human, &chunk, None).unwrap();
        let status = f.approval_params(&grant, "file_upload_status");
        assert_eq!(
            f.files.approval_step(&f.human, &status, None).unwrap()["next_offset"],
            bytes.len()
        );
        assert_eq!(
            f.files
                .approval_step(&f.human, &chunk, None)
                .unwrap_err()
                .code,
            "browser_upload_offset"
        );
        assert_eq!(
            fs::read(f.files.path(id).unwrap().join("content")).unwrap(),
            bytes
        );
        let mut finish = f.approval_params(&grant, "file_upload_finish");
        let mut wrong = f.owner.clone();
        wrong.origin = "https://other.test".into();
        assert_eq!(
            f.files
                .approval_step(&f.human, &finish, Some(&wrong))
                .unwrap_err()
                .code,
            "browser_upload_denied"
        );
        assert_eq!(
            f.files
                .upload_step(
                    &f.human,
                    "session",
                    "",
                    &json!({"action":"upload_finish","file_grant_id":id})
                )
                .unwrap_err()
                .code,
            "browser_upload_denied"
        );
        finish["action"] = "file_upload_cancel".into();
        finish["control_epoch"] = 999.into(); // Cancellation is deliberately independent of browser control.
        assert_eq!(
            f.files.approval_step(&f.human, &finish, None).unwrap()["file_deleted"],
            true
        );
        assert!(!f.files.path(id).unwrap().exists());
    }

    #[test]
    fn independent_publication_cannot_be_created_by_page_disclosure_and_consumption_is_persisted() {
        let f = Fixture::new();
        let bytes = b"single selection";
        let first = f.independent(bytes);
        let finish = f.stage(&first, bytes);
        let id = first["file_grant_id"].as_str().unwrap();
        f.files.disclosure("session", true).unwrap();
        assert_eq!(
            f.files.list(&f.owner.principal).unwrap()["uploads"],
            json!([])
        );
        assert_eq!(
            f.files
                .approval_step(
                    &f.human,
                    &f.approval_params(&first, "file_upload_status"),
                    None
                )
                .unwrap()["available_to_ai"],
            false
        );
        // Publishing one grant must not disclose another approved, hidden file.
        let second = f.independent(bytes);
        let _ = f.stage(&second, bytes);
        assert_eq!(
            f.files
                .publish_approval(&f.owner, &f.human, &finish)
                .unwrap()["available_to_ai"],
            true
        );
        let list = f.files.list(&f.owner.principal).unwrap();
        assert_eq!(list["uploads"].as_array().unwrap().len(), 1);
        assert_eq!(list["uploads"][0]["file_grant_id"], id);
        let serialized = serde_json::to_string(&list).unwrap();
        assert!(
            !serialized.contains("approval_token")
                && !serialized.contains("token_hash")
                && !serialized.contains("single selection")
        );
        f.files.disclosure("session", false).unwrap();
        assert_eq!(
            f.files.list(&f.owner.principal).unwrap()["uploads"],
            json!([])
        );
        f.files.disclosure("session", true).unwrap();
        assert_eq!(
            f.files.list(&f.owner.principal).unwrap()["uploads"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let path = f
            .files
            .consume_upload(&f.owner, id, "original-operation")
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let disk: Value = serde_json::from_slice(
            &fs::read(f.files.path(id).unwrap().join("metadata.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(disk["state"], "consumed");
        assert_eq!(disk["upload"]["operation"], "original-operation");
        for operation in ["original-operation", "another-operation"] {
            assert_eq!(
                f.files
                    .consume_upload(&f.owner, id, operation)
                    .unwrap_err()
                    .code,
                "browser_upload_denied"
            );
        }
        assert_eq!(
            f.files
                .approval_step(
                    &f.human,
                    &f.approval_params(&first, "file_upload_status"),
                    None
                )
                .unwrap()["state"],
            "consumed"
        );
        assert_eq!(
            f.files
                .approval_step(
                    &f.human,
                    &f.approval_params(&first, "file_upload_cancel"),
                    None
                )
                .unwrap_err()
                .code,
            "browser_upload_denied"
        );
        let restarted = Files::new(Some(&f.root));
        restarted.prepare().unwrap();
        assert_eq!(restarted.list(&f.human).unwrap()["uploads"], json!([]));
        assert!(!path.exists());
    }

    #[test]
    fn approval_formats_remain_distinct_and_unknown_versions_preserve_files() {
        let f = Fixture::new();
        let bytes = b"format";
        let old = f
            .begin(bytes.len() as u64, &crate::config::sha256_hex(bytes))
            .unwrap();
        let oldid = old["file_grant_id"].as_str().unwrap();
        let oldmeta: Value = serde_json::from_slice(
            &fs::read(f.files.path(oldid).unwrap().join("metadata.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(oldmeta["version"], 1);
        assert!(oldmeta["upload"].get("approval").is_none());
        let new = f.independent(bytes);
        let id = new["file_grant_id"].as_str().unwrap();
        let path = f.files.path(id).unwrap();
        let mut disk: Value =
            serde_json::from_slice(&fs::read(path.join("metadata.json")).unwrap()).unwrap();
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldUpload {
            #[serde(rename = "human_owner")]
            _human_owner: String,
            #[serde(rename = "lease")]
            _lease: String,
            #[serde(rename = "received")]
            _received: u64,
            #[serde(rename = "operation")]
            _operation: Option<String>,
        }
        assert!(serde_json::from_value::<OldUpload>(oldmeta["upload"].clone()).is_ok());
        assert!(serde_json::from_value::<OldUpload>(disk["upload"].clone()).is_err());
        disk["version"] = 3.into();
        crate::storage::write_private_replace(
            &path.join("metadata.json"),
            &serde_json::to_vec(&disk).unwrap(),
        )
        .unwrap();
        let restarted = Files::new(Some(&f.root));
        assert!(restarted.prepare().is_err());
        assert!(path.exists());
        assert!(path.join("content").exists());
    }
}
