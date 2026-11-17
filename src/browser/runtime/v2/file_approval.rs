//! Selected-session file approval uses ordinary AI read authority, never a human input permit.
use super::super::file_approval::{apply, checked_control};
use super::*;
impl Browsers {
    pub(in crate::browser::runtime) async fn v2_file_approval(
        &self,
        params: &Value,
        principal: &Principal,
        deadline: Instant,
    ) -> Result<Value> {
        let id = params["session_id"].as_str().unwrap();
        let slot = self
            .v2_sessions
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
        if session.human.is_some() || slot.human_held.load(Ordering::Acquire) {
            return Err(Failure::new("browser_paused"));
        }
        {
            let admission = self.admission.lock().unwrap();
            if !admission.online || admission.closing || slot.closing.load(Ordering::Acquire) {
                return Err(Failure::new("browser_paused"));
            }
        }
        let port = slot.port()?.for_tab(&session.active)?;
        let page = sem::Page::new(port.clone());
        let ready = || {
            if !port.connected() {
                Err(Failure::unknown("browser_lost"))
            } else if port.pending() != 0 || page.resources().1 != 0 {
                Err(Failure::unknown("browser_input_pending"))
            } else if port.dialog().is_some() {
                Err(Failure::new("browser_tab_unavailable"))
            } else {
                Ok(())
            }
        };
        ready()?;
        let origin = if params["action"] == "file_upload_chunk" {
            String::new()
        } else {
            let (_, metadata) = page.access(false)?.metadata(deadline).await?;
            crate::browser::page::origin(&metadata.url)?
        };
        let owner = files::Owner {
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
