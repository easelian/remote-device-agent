//! Authenticated, browser-only human control. Secrets are transient input, never receipts.
use super::*;
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
        let action = string(params, "action")?;
        if action.starts_with("file_upload_") {
            return self.file_approval(params, principal, deadline).await;
        }
        let keys: &[&str] = match action {
            "list" | "files_list" => &["action"],
            "file_read" => &["action", "artifact_id", "offset"],
            "file_delete" => &["action", "artifact_id"],
            "upload_begin" => &[
                "action",
                "session_id",
                "view_id",
                "lease_id",
                "control_epoch",
                "tab_id",
                "source_origin",
                "name",
                "size_bytes",
                "sha256",
                "approved",
            ],
            "upload_chunk" => &[
                "action",
                "session_id",
                "view_id",
                "lease_id",
                "control_epoch",
                "file_grant_id",
                "offset",
                "data",
            ],
            "upload_finish" | "upload_cancel" => &[
                "action",
                "session_id",
                "view_id",
                "lease_id",
                "control_epoch",
                "file_grant_id",
            ],
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
        match action {
            "list" => {
                self.prepare()?;
                Ok(json!({"sessions":self.v2_listing(principal,true)}))
            }
            "files_list" => {
                self.prepare()?;
                self.files.list(principal)
            }
            "file_read" => {
                self.prepare()?;
                self.files.read(
                    principal,
                    string(params, "artifact_id")?,
                    params["offset"]
                        .as_u64()
                        .ok_or_else(|| Failure::new("browser_invalid_params"))?,
                )
            }
            "file_delete" => {
                self.prepare()?;
                self.files.delete(principal, string(params, "artifact_id")?)
            }
            _ => self.v2_human(params, principal, deadline).await,
        }
    }
}
pub(super) fn validate_input(input: &Value) -> Result<()> {
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
