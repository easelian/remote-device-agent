//! Ownership charged before native submission, retained through cancellation.
use super::*;

pub(super) const OBJECT_LIMIT: usize = 256;
pub(super) const STREAM_LIMIT: usize = 16;
pub(super) const CLEANUP_SLOTS: usize = 4;

#[derive(Clone)]
pub(super) enum LeaseCommand {
    None,
    Object {
        session: String,
        group: String,
        resolved: bool,
    },
    Stream {
        session: String,
    },
    Release {
        session: String,
        object: String,
    },
    Group {
        session: String,
        group: String,
        through: u64,
    },
    Close {
        session: String,
        id: String,
    },
}
impl LeaseCommand {
    pub fn new(command: Command, session: Option<&str>, params: &Value, book: &Book) -> Self {
        let session = session.unwrap_or("").to_owned();
        match command {
            Command::RuntimeEvaluate(_)
            | Command::RuntimeCallFunction(_)
            | Command::DomResolveNode => {
                let group = params["objectGroup"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| {
                        params["objectId"].as_str().and_then(|id| {
                            book.objects
                                .get(&(session.clone(), id.into()))
                                .map(|v| v.group.clone())
                        })
                    })
                    .unwrap_or_default();
                Self::Object {
                    session,
                    group,
                    resolved: matches!(command, Command::DomResolveNode),
                }
            }
            Command::FetchTakeResponseBodyAsStream => Self::Stream { session },
            Command::RuntimeReleaseObject => Self::Release {
                session,
                object: params["objectId"].as_str().unwrap_or("").into(),
            },
            Command::RuntimeReleaseObjectGroup => Self::Group {
                session,
                group: params["objectGroup"].as_str().unwrap_or("").into(),
                through: book.serial,
            },
            Command::IoClose => Self::Close {
                session,
                id: params["handle"].as_str().unwrap_or("").into(),
            },
            _ => Self::None,
        }
    }
    fn capacity(&self) -> (usize, usize) {
        match self {
            Self::Object { resolved, .. } => (if *resolved { 1 } else { 2 }, 0),
            Self::Stream { .. } => (0, 1),
            _ => (0, 0),
        }
    }
    pub fn cleanup(command: Command) -> bool {
        match command {
            Command::RuntimeReleaseObject
            | Command::RuntimeReleaseObjectGroup
            | Command::IoClose => true,
            #[cfg(feature = "experimental-browser-bidi")]
            Command::CssDisable | Command::DomDisable => true,
            _ => false,
        }
    }
    pub fn object_cleanup(&self) -> bool {
        matches!(self, Self::Release { .. } | Self::Group { .. })
    }
}
struct Object {
    group: String,
    serial: u64,
}
#[derive(Clone)]
pub(super) enum Reap {
    Object { session: String, id: String },
    Stream { session: String, id: String },
}
impl Reap {
    pub fn command(&self) -> (Option<&str>, Command, Value) {
        match self {
            Self::Object { session, id } => (
                Some(session),
                Command::RuntimeReleaseObject,
                json!({"objectId":id}),
            ),
            Self::Stream { session, id } => (Some(session), Command::IoClose, json!({"handle":id})),
        }
    }
}
#[derive(Default)]
pub(super) struct Book {
    objects: HashMap<(String, String), Object>,
    streams: HashSet<(String, String)>,
    reserved_objects: usize,
    reserved_streams: usize,
    serial: u64,
    #[cfg(test)]
    peak: usize,
}
impl Book {
    pub fn retire_objects(&mut self, session: &str) {
        self.objects.retain(|(s, _), _| s != session);
    }
    pub fn owns(&self, resource: &Reap) -> bool {
        match resource {
            Reap::Object { session, id } => {
                self.objects.contains_key(&(session.clone(), id.clone()))
            }
            Reap::Stream { session, id } => self.streams.contains(&(session.clone(), id.clone())),
        }
    }
    pub fn unexpected_reply(reply: &Value) -> bool {
        let result = &reply["result"];
        result["result"]["objectId"].is_string()
            || result["object"]["objectId"].is_string()
            || result["exceptionDetails"]["exception"]["objectId"].is_string()
            || result["stream"].is_string()
    }

    pub fn reserve(&mut self, command: &LeaseCommand) -> Result<()> {
        let (objects, streams) = command.capacity();
        if self.objects.len() + self.reserved_objects + objects > OBJECT_LIMIT
            || self.streams.len() + self.reserved_streams + streams > STREAM_LIMIT
        {
            return Err(Failure::new("browser_busy"));
        }
        self.reserved_objects += objects;
        self.reserved_streams += streams;
        #[cfg(test)]
        {
            self.peak = self.peak.max(
                self.objects.len()
                    + self.streams.len()
                    + self.reserved_objects
                    + self.reserved_streams,
            );
        }
        Ok(())
    }
    pub fn cancel_unsent(&mut self, command: &LeaseCommand) {
        let (objects, streams) = command.capacity();
        self.reserved_objects -= objects;
        self.reserved_streams -= streams;
    }
    pub fn settle(&mut self, command: &LeaseCommand, reply: &Value) -> Vec<Reap> {
        self.cancel_unsent(command);
        let mut created = Vec::new();
        if reply.get("error").is_some() {
            return created;
        }
        let payload = &reply["result"];
        match command {
            LeaseCommand::Object {
                session,
                group,
                resolved,
            } => {
                let mut remotes = vec![&payload[if *resolved { "object" } else { "result" }]];
                if !*resolved {
                    remotes.push(&payload["exceptionDetails"]["exception"]);
                }
                for remote in remotes {
                    if let Some(id) = remote["objectId"].as_str() {
                        self.serial += 1;
                        if self
                            .objects
                            .insert(
                                (session.clone(), id.into()),
                                Object {
                                    group: group.clone(),
                                    serial: self.serial,
                                },
                            )
                            .is_none()
                        {
                            created.push(Reap::Object {
                                session: session.clone(),
                                id: id.into(),
                            });
                        }
                    }
                }
            }
            LeaseCommand::Stream { session } => {
                if let Some(id) = payload["stream"].as_str()
                    && self.streams.insert((session.clone(), id.into()))
                {
                    created.push(Reap::Stream {
                        session: session.clone(),
                        id: id.into(),
                    });
                }
            }
            LeaseCommand::Release { session, object } => {
                self.objects.remove(&(session.clone(), object.clone()));
            }
            LeaseCommand::Group {
                session,
                group,
                through,
            } => self
                .objects
                .retain(|(s, _), v| s != session || &v.group != group || v.serial > *through),
            LeaseCommand::Close { session, id } => {
                self.streams.remove(&(session.clone(), id.clone()));
            }
            LeaseCommand::None => {}
        }
        created
    }
    pub fn event(&mut self, event: &Value) -> bool {
        let session = event["sessionId"].as_str().unwrap_or("");
        let params = &event["params"];
        match event["method"].as_str() {
            // Runtime event reporting is deliberately not enabled. Unexpected
            // native ownership must not bypass command admission.
            Some("Runtime.consoleAPICalled") => !params["args"]
                .as_array()
                .is_some_and(|args| args.iter().any(|v| v["objectId"].is_string())),
            Some("Runtime.exceptionThrown") => {
                !params["exceptionDetails"]["exception"]["objectId"].is_string()
            }
            Some("Runtime.inspectRequested") => !params["object"]["objectId"].is_string(),
            Some("Runtime.executionContextsCleared") => {
                self.objects.retain(|(s, _), _| s != session);
                true
            }
            Some("Target.detachedFromTarget") => {
                let id = params["sessionId"].as_str().unwrap_or("");
                self.objects.retain(|(s, _), _| s != id);
                true
            }
            _ => true,
        }
    }
    #[cfg(test)]
    pub fn snapshot(&self) -> Value {
        json!({"objects":self.objects.len(),"streams":self.streams.len(),"reserved_objects":self.reserved_objects,"reserved_streams":self.reserved_streams,"charged_peak":self.peak,"object_limit":OBJECT_LIMIT,"stream_limit":STREAM_LIMIT,"total_limit":OBJECT_LIMIT+STREAM_LIMIT})
    }
}
