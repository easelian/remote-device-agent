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
    pub object_peak: usize,
    pub stream_peak: usize,
    pub total_peak: usize,
    pub group_peaks: std::collections::BTreeMap<String, usize>,
    pub objects_created: usize,
    pub streams_created: usize,
    pub unsolicited_objects: usize,
}
impl Resources {
    pub fn retire_objects(&mut self, session: &str) {
        self.objects.retain(|(s, _), _| s != session);
    }
    fn object(&mut self, session: &str, group: &str, remote: &Value) {
        if let Some(id) = remote["objectId"].as_str() {
            if self
                .objects
                .insert((session.into(), id.into()), group.into())
                .is_none()
            {
                self.objects_created += 1;
            }
            self.peaks();
        }
    }
    fn peaks(&mut self) {
        self.object_peak = self.object_peak.max(self.objects.len());
        self.stream_peak = self.stream_peak.max(self.streams.len());
        self.total_peak = self.total_peak.max(self.objects.len() + self.streams.len());
        let mut groups = std::collections::BTreeMap::<String, usize>::new();
        for group in self.objects.values() {
            let category = if group.starts_with("rdh-reader-") {
                "reader"
            } else if group.is_empty() {
                "ungrouped"
            } else {
                group
            };
            *groups.entry(category.into()).or_default() += 1;
        }
        for (group, count) in groups {
            let peak = self.group_peaks.entry(group).or_default();
            *peak = (*peak).max(count);
        }
    }
    pub fn snapshot(&self) -> Value {
        json!({"objects":self.objects.len(),"streams":self.streams.len(),"object_peak":self.object_peak,
            "stream_peak":self.stream_peak,"total_peak":self.total_peak,"group_peaks":self.group_peaks,
            "objects_created":self.objects_created,"streams_created":self.streams_created,
            "unsolicited_objects":self.unsolicited_objects})
    }
    pub fn event(&mut self, event: &Value) {
        let session = event["sessionId"].as_str().unwrap_or("");
        let before = self.objects_created;
        match event["method"].as_str() {
            Some("Runtime.consoleAPICalled") => {
                if let Some(args) = event["params"]["args"].as_array() {
                    for remote in args {
                        self.object(session, "", remote);
                    }
                }
            }
            Some("Runtime.exceptionThrown") => self.object(
                session,
                "",
                &event["params"]["exceptionDetails"]["exception"],
            ),
            Some("Runtime.inspectRequested") => {
                self.object(session, "", &event["params"]["object"])
            }
            Some("Runtime.executionContextsCleared") => {
                self.objects.retain(|(s, _), _| s != session)
            }
            Some("Target.detachedFromTarget") => {
                let detached = event["params"]["sessionId"].as_str().unwrap_or("");
                self.objects.retain(|(s, _), _| s != detached);
            }
            _ => {}
        }
        self.unsolicited_objects += self.objects_created - before;
    }
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
                self.object(
                    session,
                    group,
                    &reply[if *resolved { "object" } else { "result" }],
                );
                self.object(session, group, &reply["exceptionDetails"]["exception"]);
            }
            Command::Release { session, object } => {
                self.objects.remove(&(session.clone(), object.clone()));
            }
            Command::Group { session, group } => {
                self.objects.retain(|(s, _), g| s != session || g != group);
            }
            Command::Stream => {
                if let Some(id) = reply["stream"].as_str() {
                    if self.streams.insert(id.into()) {
                        self.streams_created += 1;
                    }
                    self.peaks();
                }
            }
            Command::CloseStream(id) => {
                self.streams.remove(id);
            }
        }
    }
}
