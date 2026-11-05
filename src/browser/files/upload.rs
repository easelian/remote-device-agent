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
            version: 1,
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
            upload: Some(Upload {
                human_owner: human.owner.clone(),
                lease: lease.into(),
                received: 0,
                operation: None,
            }),
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
                    && e.meta.upload.as_ref().is_some_and(|u| {
                        u.human_owner == human.owner && u.lease == lease && u.operation.is_none()
                    })
            })
            .ok_or_else(|| Failure::new("browser_upload_denied"))?;
        if params["action"] == "upload_cancel" {
            fs::remove_dir_all(&path).map_err(|_| Failure::unknown("browser_io_error"))?;
            catalog.entries.remove(id);
            return Ok(json!({"file_grant_id":id,"file_deleted":true}));
        }
        if entry.meta.state != "staging" {
            return Err(Failure::new("browser_upload_denied"));
        }
        let upload = entry.meta.upload.as_mut().unwrap();
        if params["action"] == "upload_chunk" {
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
        if params["action"] != "upload_finish" || upload.received != entry.meta.size_bytes {
            return Err(Failure::new("browser_file_incomplete"));
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
}
