//! The reader owns native session admission, independently of event consumers.
use super::*;

pub(super) const MAX_SESSIONS: usize = 128;
pub(super) const MAX_SESSION_ID_BYTES: usize = 256;
pub(super) const MAX_SESSION_BYTES: usize = 32 * 1024;
pub(super) const DETACH_SETTLEMENT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct Sessions {
    active: HashMap<String, Option<String>>,
    bytes: usize,
    #[cfg(test)]
    peak: (usize, usize),
}

impl Sessions {
    fn valid(id: &str) -> bool {
        !id.is_empty() && id.len() <= MAX_SESSION_ID_BYTES
    }

    pub fn contains(&self, session: &str) -> bool {
        self.active.contains_key(session)
    }

    pub fn attach(&mut self, id: &str, parent: Option<&str>) -> bool {
        let bytes = id.len() + parent.map_or(0, str::len);
        if !Self::valid(id)
            || parent.is_some_and(|parent| !self.contains(parent))
            || self.contains(id)
            || self.active.len() == MAX_SESSIONS
            || self.bytes + bytes > MAX_SESSION_BYTES
        {
            return false;
        }
        self.active.insert(id.into(), parent.map(str::to_owned));
        self.bytes += bytes;
        #[cfg(test)]
        {
            self.peak.0 = self.peak.0.max(self.active.len());
            self.peak.1 = self.peak.1.max(self.bytes);
        }
        true
    }

    fn detach(&mut self, id: &str) -> Vec<String> {
        let mut retired = vec![id.to_owned()];
        let mut cursor = 0;
        while cursor < retired.len() {
            let children: Vec<_> = self
                .active
                .iter()
                .filter(|(_, parent)| parent.as_deref() == Some(retired[cursor].as_str()))
                .map(|(id, _)| id.clone())
                .collect();
            retired.extend(children);
            if let Some(parent) = self.active.remove(&retired[cursor]) {
                self.bytes -= retired[cursor].len() + parent.as_deref().map_or(0, str::len);
            }
            cursor += 1;
        }
        retired
    }

    pub fn clear(&mut self) {
        self.active.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Value {
        json!({"count":self.active.len(),"bytes":self.bytes,
            "peak_count":self.peak.0,"peak_bytes":self.peak.1,
            "count_limit":MAX_SESSIONS,"byte_limit":MAX_SESSION_BYTES,
            "id_byte_limit":MAX_SESSION_ID_BYTES})
    }
}

impl Inner {
    pub(super) fn session_event(&self, pending: &mut PendingState, event: &Value) -> bool {
        match event["method"].as_str() {
            Some("Target.attachedToTarget") => {
                let Some(id) = event["params"]["sessionId"].as_str() else {
                    return false;
                };
                let parent = match event.get("sessionId") {
                    None => None,
                    Some(Value::String(parent)) => Some(parent.as_str()),
                    Some(_) => return false,
                };
                // An attached ID cannot alias an outstanding command from a
                // retired session. CDP defines IDs as unique per connection.
                if pending
                    .commands
                    .values()
                    .any(|command| command.session.as_deref() == Some(id))
                {
                    return false;
                }
                pending.sessions.attach(id, parent)
            }
            Some("Target.detachedFromTarget") => {
                let Some(id) = event["params"]["sessionId"]
                    .as_str()
                    .filter(|id| Sessions::valid(id))
                else {
                    return false;
                };
                let retired = pending.sessions.detach(id);
                let deadline = tokio::time::Instant::now() + DETACH_SETTLEMENT;
                for session in &retired {
                    pending.dialogs.remove(session);
                    pending.leases.retire_objects(session);
                    #[cfg(test)]
                    pending.resources.retire_objects(session);
                }
                for command in pending.commands.values_mut().filter(|command| {
                    command
                        .session
                        .as_ref()
                        .is_some_and(|session| retired.contains(session))
                }) {
                    // A detach is not cancellation of JavaScript or input.
                    // Keep slots, reservations and input barriers until settled.
                    command.retired_deadline.get_or_insert(deadline);
                    if let Some(reply) = command.reply.take() {
                        let _ = reply.send(Err(Failure::unknown("browser_lost")));
                    }
                }
                true
            }
            _ => true,
        }
    }
}

#[cfg(test)]
impl Cdp {
    /// Establish already attached contexts for protocol tests about other bounds.
    pub fn test_sessions(&self, sessions: &[&str]) {
        let mut pending = self.inner.pending.lock().unwrap();
        for session in sessions {
            assert!(pending.sessions.attach(session, None));
        }
    }
}
