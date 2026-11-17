//! User file authorization is independent of browser input ownership.
use super::*;

fn token(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn identifier(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    })
}
fn validate(params: &Value) -> Result<&str> {
    let action = params["action"].as_str().unwrap_or("");
    let keys: &[&str] = match action {
        "file_upload_context" => &["action", "session_id", "view_id"],
        "file_upload_begin" => &[
            "action",
            "session_id",
            "view_id",
            "control_epoch",
            "tab_id",
            "source_origin",
            "name",
            "size_bytes",
            "sha256",
            "approved",
        ],
        "file_upload_chunk" => &[
            "action",
            "session_id",
            "view_id",
            "control_epoch",
            "file_grant_id",
            "approval_token",
            "offset",
            "data",
        ],
        "file_upload_finish" => &[
            "action",
            "session_id",
            "view_id",
            "control_epoch",
            "file_grant_id",
            "approval_token",
        ],
        "file_upload_status" | "file_upload_cancel" => &[
            "action",
            "session_id",
            "view_id",
            "file_grant_id",
            "approval_token",
        ],
        _ => return Err(Failure::new("browser_invalid_params")),
    };
    let valid = params
        .as_object()
        .is_some_and(|m| m.len() == keys.len() && m.keys().all(|k| keys.contains(&k.as_str())))
        && identifier(&params["session_id"])
        && token(&params["view_id"])
        && (!keys.contains(&"control_epoch")
            || params["control_epoch"].as_u64().is_some_and(|e| e > 0))
        && (!keys.contains(&"file_grant_id")
            || token(&params["file_grant_id"]) && token(&params["approval_token"]));
    if !valid {
        return Err(Failure::new("browser_invalid_params"));
    }
    if action == "file_upload_begin" {
        if !identifier(&params["tab_id"])
            || params["approved"] != true
            || !params["source_origin"]
                .as_str()
                .is_some_and(|s| s.len() <= 2048 && page::origin(s).is_ok_and(|origin| origin == s))
            || !params["name"].as_str().is_some_and(|s| {
                !s.trim().is_empty()
                    && !matches!(s, "." | "..")
                    && s.len() <= 240
                    && !s.chars().any(|c| c.is_control() || matches!(c, '/' | '\\'))
            })
            || !params["sha256"].as_str().is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(Failure::new("browser_invalid_params"));
        }
        if params["size_bytes"]
            .as_u64()
            .is_none_or(|v| v > files::MAX_FILE)
        {
            return Err(Failure::new("browser_file_limit"));
        }
    }
    if action == "file_upload_chunk"
        && (params["offset"]
            .as_u64()
            .is_none_or(|v| v > files::MAX_FILE)
            || params["data"]
                .as_str()
                .is_none_or(|s| s.len() > files::CHUNK.div_ceil(3) * 4))
    {
        return Err(Failure::new("browser_invalid_params"));
    }
    Ok(action)
}

pub(super) fn checked_control(control: &Control, params: &Value, epoch: u64) -> Result<()> {
    if control.snapshot() != (epoch, false) {
        return Err(Failure::new("browser_paused"));
    }
    if params["action"] != "file_upload_context" && params["control_epoch"].as_u64() != Some(epoch)
    {
        return Err(Failure::new("browser_control_changed"));
    }
    Ok(())
}

pub(super) fn apply(
    files: &Files,
    owner: &files::Owner,
    human: &Principal,
    params: &Value,
    epoch: u64,
    deadline: Instant,
    ready: impl Fn() -> Result<()>,
) -> Result<Value> {
    checked_control(&owner.control, params, epoch)?;
    ready()?;
    let action = params["action"].as_str().unwrap();
    if Instant::now() >= deadline {
        return Err(Failure::new("browser_timeout_unknown"));
    }
    let result = match action {
        "file_upload_context" => json!({"file_authorization_version":1,"session_id":owner.session,
            "tab_id":owner.tab,"source_origin":owner.origin,"control_epoch":epoch,"controller":"ai",
            "max_file_bytes":files::MAX_FILE,"max_total_bytes":files::MAX_TOTAL,"max_files":files::MAX_FILES,
            "max_chunk_bytes":files::CHUNK,"grant_ttl_seconds":files::UPLOAD_TTL,"uses":1}),
        "file_upload_begin" => {
            if params["tab_id"] != owner.tab || params["source_origin"] != owner.origin {
                return Err(Failure::new("browser_upload_denied"));
            }
            files.approval_begin(owner, human, params)?
        }
        "file_upload_chunk" => files.approval_step(human, params, None)?,
        "file_upload_finish" => files.approval_step(human, params, Some(owner))?,
        _ => return Err(Failure::new("browser_invalid_params")),
    };
    // Hashing, disk writes and renames above do not hold the control mutex.
    // Claims can fence them, but cannot make the prepared file visible to AI.
    let publication = (|| {
        ready()?;
        if Instant::now() >= deadline {
            return Err(Failure::unknown("browser_timeout_unknown"));
        }
        let state = owner.control.state.lock().unwrap();
        if *state != (epoch, false) {
            return Err(Failure::unknown("browser_control_changed"));
        }
        if action == "file_upload_finish" {
            files.publish_approval(owner, human, params)
        } else {
            Ok(result.clone())
        }
    })();
    if publication.is_err() && action == "file_upload_begin" {
        // No token was delivered. Remove this exact empty reservation when
        // possible; failed cleanup keeps its original bounded reservation/TTL.
        let cancel = json!({"action":"file_upload_cancel","session_id":owner.session,"view_id":params["view_id"],
            "file_grant_id":result["file_grant_id"],"approval_token":result["approval_token"]});
        if files.approval_step(human, &cancel, None).is_err() {
            return Err(Failure::unknown("browser_cleanup_pending"));
        }
    }
    publication
}

impl Browsers {
    pub(super) async fn file_approval(
        &self,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
    ) -> Result<Value> {
        let action = validate(params)?;
        if principal.channel != "human" {
            return Err(Failure::new("browser_unauthorized"));
        }
        if !self.settings.uploads {
            return Err(Failure::new("browser_upload_disabled"));
        }
        if self.info()["extensions"]["file_authorization"]["available"] != true {
            return Err(Failure::new("browser_feature_unsupported"));
        }
        self.prepare_storage()?;
        let value = if matches!(action, "file_upload_status" | "file_upload_cancel") {
            // These operations retain the original grant authority after the
            // browser session has closed or another controller has taken over.
            self.files.approval_step(principal, params, None)?
        } else {
            #[cfg(all(
                target_os = "linux",
                target_arch = "x86_64",
                feature = "experimental-browser-bidi"
            ))]
            if self.v2_contains(params) {
                return self
                    .v2_file_approval(params, principal, deadline)
                    .await
                    .and_then(bounded);
            }
            self.v1_file_approval(params, principal, deadline).await?
        };
        bounded(value)
    }
    async fn v1_file_approval(
        &self,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
    ) -> Result<Value> {
        let id = params["session_id"].as_str().unwrap();
        let slot = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.user == principal.user)
            .cloned()
            .ok_or_else(|| Failure::new("browser_session_unavailable"))?;
        let session = slot
            .session
            .try_lock()
            .map_err(|_| Failure::new("browser_busy"))?;
        let epoch = slot.control.snapshot().0;
        checked_control(&slot.control, params, epoch)?;
        if session.human.is_some() {
            return Err(Failure::new("browser_paused"));
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
        let engine = process.engine.authorized(slot.control.clone(), epoch);
        let ready = || {
            if !engine.connected() {
                Err(Failure::unknown("browser_lost"))
            } else if engine.pending() != 0 {
                Err(Failure::unknown("browser_input_pending"))
            } else {
                Ok(())
            }
        };
        ready()?;
        let target = session
            .targets
            .lock()
            .unwrap()
            .get(&session.active)
            .filter(|t| t.ready && t.kind == ContextKind::Page && t.dialog.is_none())
            .cloned()
            .ok_or_else(|| Failure::new("browser_tab_unavailable"))?;
        let origin = if params["action"] == "file_upload_chunk" {
            String::new()
        } else {
            let frames = page::frame_tree(&engine, &target.session, deadline).await?;
            let metadata = page::metadata(
                &engine,
                frames
                    .first()
                    .ok_or_else(|| Failure::new("browser_tab_unavailable"))?,
                deadline,
            )
            .await?;
            page::origin(metadata["url"].as_str().unwrap_or(""))?
        };
        let owner = files::Owner {
            principal: Principal {
                version: 1,
                owner: slot.owner.clone(),
                user: slot.user.clone(),
                channel: "ai".into(),
            },
            session: id.into(),
            tab: target.id,
            origin,
            control: slot.control.clone(),
        };
        apply(
            &self.files,
            &owner,
            principal,
            params,
            epoch,
            deadline,
            ready,
        )
    }
}
fn bounded(value: Value) -> Result<Value> {
    if serde_json::to_vec(&value)
        .map_err(super::super::io_error)?
        .len()
        > 4096
    {
        Err(Failure::new("browser_file_limit"))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_authorization_never_accepts_a_browser_control_lease_or_caller_identity() {
        let context =
            json!({"action":"file_upload_context","session_id":"session","view_id":"a".repeat(32)});
        assert!(validate(&context).is_ok());
        for (key, value) in [
            ("lease_id", json!("lease")),
            ("browser_principal", json!({"channel":"human"})),
            ("control_epoch", json!(1)),
        ] {
            let mut forged = context.clone();
            forged[key] = value;
            assert!(validate(&forged).is_err());
        }
        let cancel = json!({"action":"file_upload_cancel","session_id":"session","view_id":"a".repeat(32),
            "file_grant_id":"b".repeat(32),"approval_token":"c".repeat(32)});
        assert!(validate(&cancel).is_ok());
        let mut old = cancel.clone();
        old["lease_id"] = old["approval_token"].take();
        assert!(validate(&old).is_err());
    }

    #[test]
    fn takeover_between_file_validation_and_publication_keeps_the_approval_hidden() {
        use base64::Engine as _;
        use std::sync::atomic::AtomicUsize;
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Directory(std::env::temp_dir().join(format!(
            "rdh-approval-fence-{}",
            crate::random::hex_token::<8>().unwrap()
        )));
        crate::storage::create_private_dir(&root.0).unwrap();
        let files = Files::new(Some(&root.0));
        files.prepare().unwrap();
        let owner = files::Owner {
            principal: Principal {
                version: 1,
                owner: "a".repeat(64),
                user: "b".repeat(64),
                channel: "ai".into(),
            },
            session: "session".into(),
            tab: "tab".into(),
            origin: "https://fixture.test".into(),
            control: Arc::new(Control::new()),
        };
        let human = Principal {
            owner: "c".repeat(64),
            channel: "human".into(),
            ..owner.principal.clone()
        };
        let bytes = b"approved bytes";
        let begin = json!({"action":"file_upload_begin","session_id":"session","view_id":"d".repeat(32),
            "tab_id":"tab","source_origin":owner.origin,"control_epoch":1,"name":"fixture.bin","size_bytes":bytes.len(),
            "sha256":crate::config::sha256_hex(bytes),"approved":true});
        validate(&begin).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let grant = apply(&files, &owner, &human, &begin, 1, deadline, || Ok(())).unwrap();
        let mut params = json!({"action":"file_upload_chunk","session_id":"session","view_id":"d".repeat(32),
            "control_epoch":1,"file_grant_id":grant["file_grant_id"],"approval_token":grant["approval_token"],
            "offset":0,"data":base64::engine::general_purpose::STANDARD.encode(bytes)});
        validate(&params).unwrap();
        apply(&files, &owner, &human, &params, 1, deadline, || Ok(())).unwrap();
        params["action"] = "file_upload_finish".into();
        params.as_object_mut().unwrap().remove("offset");
        params.as_object_mut().unwrap().remove("data");
        validate(&params).unwrap();
        let calls = AtomicUsize::new(0);
        let error = apply(&files, &owner, &human, &params, 1, deadline, || {
            if calls.fetch_add(1, Ordering::AcqRel) == 1 {
                owner.control.change(true);
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.code, "browser_control_changed");
        assert_eq!(error.state, "unknown");
        assert_eq!(owner.control.snapshot(), (2, true));
        assert_eq!(files.list(&owner.principal).unwrap()["uploads"], json!([]));
        files.disclosure("session", true).unwrap();
        assert_eq!(files.list(&owner.principal).unwrap()["uploads"], json!([]));
        params["action"] = "file_upload_status".into();
        params.as_object_mut().unwrap().remove("control_epoch");
        validate(&params).unwrap();
        let original = files.approval_step(&human, &params, None).unwrap();
        assert_eq!(original["state"], "approved");
        assert_eq!(original["available_to_ai"], false);
        params["action"] = "file_upload_cancel".into();
        assert_eq!(
            files.approval_step(&human, &params, None).unwrap()["file_deleted"],
            true
        );
    }
}
