//! Test-only counters from actual replies, including replies to cancelled
//! callers. The release binary has no instrumentation or diagnostic endpoint.
use super::*;
#[derive(Clone)]
pub(super) enum Command {
    None,
    Object {
        session: String,
        group: String,
        resolved: bool,
    },
    Release {
        session: String,
        object: String,
    },
    Group {
        session: String,
        group: String,
    },
    Stream,
    CloseStream(String),
}
impl Command {
    pub(super) fn new(session: Option<&str>, method: &str, params: &Value) -> Self {
        let session = session.unwrap_or("").to_owned();
        match method {
            "Runtime.evaluate" | "Runtime.callFunctionOn" | "DOM.resolveNode" => Self::Object {
                session,
                group: params["objectGroup"].as_str().unwrap_or("").into(),
                resolved: method == "DOM.resolveNode",
            },
            "Runtime.releaseObject" => Self::Release {
                session,
                object: params["objectId"].as_str().unwrap_or("").into(),
            },
            "Runtime.releaseObjectGroup" => Self::Group {
                session,
                group: params["objectGroup"].as_str().unwrap_or("").into(),
            },
            "Fetch.takeResponseBodyAsStream" => Self::Stream,
            "IO.close" => Self::CloseStream(params["handle"].as_str().unwrap_or("").into()),
            _ => Self::None,
        }
    }
}
#[derive(Default)]
pub(super) struct Resources {
    pub objects: HashMap<(String, String), String>,
    pub streams: HashSet<String>,
}
impl Resources {
    pub(super) fn settle(&mut self, command: &Command, reply: &Value) {
        if reply.get("error").is_some() {
            return;
        }
        let reply = &reply["result"];
        match command {
            Command::None => {}
            Command::Object {
                session,
                group,
                resolved,
            } => {
                if let Some(id) =
                    reply[if *resolved { "object" } else { "result" }]["objectId"].as_str()
                {
                    self.objects
                        .insert((session.clone(), id.into()), group.clone());
                }
            }
            Command::Release { session, object } => {
                self.objects.remove(&(session.clone(), object.clone()));
            }
            Command::Group { session, group } => {
                self.objects.retain(|(s, _), g| s != session || g != group);
            }
            Command::Stream => {
                if let Some(id) = reply["stream"].as_str() {
                    self.streams.insert(id.into());
                }
            }
            Command::CloseStream(id) => {
                self.streams.remove(id);
            }
        }
    }
}
