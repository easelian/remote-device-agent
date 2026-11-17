use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const CORE_EVENTS: [&str; 2] = [
    "browsingContext.contextCreated",
    "browsingContext.contextDestroyed",
];
pub(super) const PAGE_EVENTS: [&str; 8] = [
    "network.beforeRequestSent",
    "network.responseStarted",
    "browsingContext.downloadWillBegin",
    "browsingContext.downloadEnd",
    "script.realmDestroyed",
    "browsingContext.userPromptOpened",
    "browsingContext.userPromptClosed",
    "browsingContext.navigationCommitted",
];
pub(super) const MAX_CONTEXTS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(super) struct ContextId {
    scope: u64,
    key: String,
}
impl ContextId {
    pub fn parse(scope: u64, value: &Value) -> Option<Self> {
        let text = value.as_str()?;
        (!text.is_empty() && text.len() <= 256).then(|| Self {
            scope,
            key: text.to_owned(),
        })
    }
    pub fn as_str(&self) -> &str {
        &self.key
    }
    pub fn belongs_to(&self, scope: u64) -> bool {
        self.scope == scope
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Event {
    Created {
        context: ContextId,
        parent: Option<ContextId>,
    },
    Destroyed {
        context: ContextId,
    },
    Request {
        context: Option<ContextId>,
        request: String,
        url: String,
        navigation: Option<String>,
        hop: u32,
        blocked: bool,
    },
    Response {
        context: Option<ContextId>,
        request: String,
        url: String,
        navigation: Option<String>,
        status: u64,
        headers: Vec<(String, String)>,
        blocked: bool,
    },
    Download {
        context: ContextId,
        navigation: Option<String>,
        url: String,
        id: String,
        end: bool,
        path: Option<String>,
    },
    RealmDestroyed {
        realm: String,
    },
    Prompt {
        context: ContextId,
        kind: String,
        message: String,
    },
    PromptClosed {
        context: ContextId,
    },
    Navigation {
        context: ContextId,
        navigation: String,
    },
}
impl Event {
    pub fn parse(scope: u64, method: &str, params: &Value) -> Option<Self> {
        let bounded = |value: &Value, max| {
            value
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= max)
                .map(str::to_owned)
        };
        if method == "network.responseStarted" {
            let headers = params["response"]["headers"].as_array()?;
            if headers.len() > 128 {
                return None;
            }
            let headers = headers
                .iter()
                .map(|h| {
                    Some((
                        bounded(&h["name"], 512)?,
                        match h["value"]["type"].as_str() {
                            Some("string") => h["value"]["value"]
                                .as_str()
                                .filter(|s| s.len() <= 8192)?
                                .to_owned(),
                            Some("base64") => {
                                use base64::Engine;
                                let text =
                                    h["value"]["value"].as_str().filter(|s| s.len() <= 10924)?;
                                let bytes = base64::engine::general_purpose::STANDARD
                                    .decode(text)
                                    .ok()?;
                                if bytes.len() > 8192 {
                                    return None;
                                }
                                bytes.into_iter().map(char::from).collect()
                            }
                            _ => return None,
                        },
                    ))
                })
                .collect::<Option<Vec<_>>>()?;
            return Some(Self::Response {
                context: if params["context"].is_null() {
                    None
                } else {
                    Some(ContextId::parse(scope, &params["context"])?)
                },
                request: bounded(&params["request"]["request"], 512)?,
                url: bounded(&params["request"]["url"], 8192)?,
                navigation: if params["navigation"].is_null() {
                    None
                } else {
                    Some(bounded(&params["navigation"], 512)?)
                },
                status: params["response"]["status"].as_u64()?,
                headers,
                blocked: params["isBlocked"].as_bool()?,
            });
        }
        if matches!(
            method,
            "browsingContext.downloadWillBegin" | "browsingContext.downloadEnd"
        ) {
            let end = method == "browsingContext.downloadEnd";
            if end && !matches!(params["status"].as_str(), Some("complete" | "canceled")) {
                return None;
            }
            return Some(Self::Download {
                context: ContextId::parse(scope, &params["context"])?,
                navigation: if params["navigation"].is_null() {
                    None
                } else {
                    Some(bounded(&params["navigation"], 512)?)
                },
                url: bounded(&params["url"], 8192)?,
                id: bounded(&params["download"], 512)?,
                end,
                path: if end && params["status"] == "complete" {
                    Some(bounded(&params["filepath"], 8192)?)
                } else {
                    None
                },
            });
        }
        if method == "network.beforeRequestSent" {
            let bounded = |value: &Value, max| {
                value
                    .as_str()
                    .filter(|v| !v.is_empty() && v.len() <= max)
                    .map(str::to_owned)
            };
            return Some(Self::Request {
                context: if params["context"].is_null() {
                    None
                } else {
                    Some(ContextId::parse(scope, &params["context"])?)
                },
                request: bounded(&params["request"]["request"], 512)?,
                url: bounded(&params["request"]["url"], 8192)?,
                navigation: if params["navigation"].is_null() {
                    None
                } else {
                    Some(bounded(&params["navigation"], 512)?)
                },
                hop: params["redirectCount"].as_u64()?.try_into().ok()?,
                blocked: params["isBlocked"].as_bool()?,
            });
        }
        if method == "script.realmDestroyed" {
            return Some(Self::RealmDestroyed {
                realm: params["realm"]
                    .as_str()
                    .filter(|v| !v.is_empty() && v.len() <= 512)?
                    .to_owned(),
            });
        }
        let context = ContextId::parse(scope, &params["context"])?;
        match method {
            "browsingContext.contextCreated" => Some(Self::Created {
                context,
                parent: if params["parent"].is_null() {
                    None
                } else {
                    Some(ContextId::parse(scope, &params["parent"])?)
                },
            }),
            "browsingContext.contextDestroyed" => Some(Self::Destroyed { context }),
            "browsingContext.userPromptOpened" => Some(Self::Prompt {
                context,
                kind: params["type"].as_str()?.to_owned(),
                message: params["message"].as_str()?.chars().take(4096).collect(),
            }),
            "browsingContext.userPromptClosed" => Some(Self::PromptClosed { context }),
            "browsingContext.navigationCommitted" => Some(Self::Navigation {
                context,
                navigation: params["navigation"]
                    .as_str()
                    .filter(|value| !value.is_empty() && value.len() <= 512)?
                    .to_owned(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
#[test]
fn response_headers_preserve_bounded_binary_bytes() {
    let mut params = serde_json::json!({"context":"tab","request":{"request":"request","url":"https://example.test/file"},"navigation":"nav","response":{"status":200,"headers":[{"name":"Content-Length","value":{"type":"base64","value":"MTIz"}},{"name":"X-Binary","value":{"type":"base64","value":"/w=="}}]},"isBlocked":true});
    let Some(Event::Response { headers, .. }) = Event::parse(1, "network.responseStarted", &params)
    else {
        panic!("valid native byte headers");
    };
    assert_eq!(
        headers,
        vec![
            ("Content-Length".into(), "123".into()),
            ("X-Binary".into(), "ÿ".into())
        ]
    );
    params["response"]["headers"][0]["value"]["value"] = "!invalid!".into();
    assert!(Event::parse(1, "network.responseStarted", &params).is_none());
}

/// Startup-only ownership of the initial context snapshot. Replaying the
/// bounded subscription history in arrival order prevents a stale getTree
/// result from resurrecting a context destroyed while that query was pending.
pub(super) struct Contexts(
    BTreeMap<ContextId, Option<ContextId>>,
    BTreeSet<ContextId>,
    u64,
);
impl Contexts {
    pub fn snapshot(scope: u64, value: &Value) -> Option<Self> {
        let mut result = Self(BTreeMap::new(), BTreeSet::new(), scope);
        result.append(value["contexts"].as_array()?, 0)?;
        Some(result)
    }
    fn append(&mut self, values: &[Value], depth: usize) -> Option<()> {
        if depth > 32 {
            return None;
        }
        for value in values {
            let context = ContextId::parse(self.2, &value["context"])?;
            let parent = if value["parent"].is_null() {
                None
            } else {
                Some(ContextId::parse(self.2, &value["parent"])?)
            };
            if self.0.insert(context, parent).is_some() || self.0.len() > MAX_CONTEXTS {
                return None;
            }
            if !value["children"].is_null() {
                self.append(value["children"].as_array()?, depth + 1)?;
            }
        }
        Some(())
    }
    #[cfg(target_os = "linux")]
    pub fn roots(&self) -> Vec<ContextId> {
        self.0
            .iter()
            .filter(|(_, parent)| parent.is_none())
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub fn apply(&mut self, event: &Event) -> bool {
        match event {
            Event::Created { context, parent } => {
                if !context.belongs_to(self.2)
                    || parent.as_ref().is_some_and(|id| !id.belongs_to(self.2))
                {
                    return false;
                }
                if self.1.contains(context) || parent.as_ref().is_some_and(|id| self.1.contains(id))
                {
                    return false;
                }
                self.0.insert(context.clone(), parent.clone());
            }
            Event::Destroyed { context } => {
                if !context.belongs_to(self.2) {
                    return false;
                }
                self.0.remove(context);
                self.1.insert(context.clone());
                // A parent's destruction invalidates its descendants even if
                // separate child notifications have not arrived yet.
                let mut removed = vec![context.clone()];
                while let Some(parent) = removed.pop() {
                    let children: Vec<_> = self
                        .0
                        .iter()
                        .filter(|(_, p)| p.as_ref() == Some(&parent))
                        .map(|(id, _)| id.clone())
                        .collect();
                    for child in children {
                        self.0.remove(&child);
                        self.1.insert(child.clone());
                        removed.push(child);
                    }
                }
            }
            _ => return true,
        }
        self.0.len() <= MAX_CONTEXTS && self.1.len() <= MAX_CONTEXTS
    }
    pub fn ids(&self) -> Vec<ContextId> {
        self.0.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn startup_history_removes_destroyed_contexts_and_deduplicates_creation() {
        let mut contexts = Contexts::snapshot(1,&json!({"contexts":[{"context":"old","children":[{"context":"child","parent":"old"}]}]})).unwrap();
        let event = |method, params| Event::parse(1, method, &params).unwrap();
        contexts.apply(&event(
            CORE_EVENTS[0],
            json!({"context":"new","parent":null}),
        ));
        contexts.apply(&event(
            CORE_EVENTS[0],
            json!({"context":"new","parent":null}),
        ));
        contexts.apply(&event(CORE_EVENTS[1], json!({"context":"old"})));
        assert_eq!(
            contexts.ids(),
            vec![ContextId::parse(1, &json!("new")).unwrap()]
        );
        contexts.apply(&event(CORE_EVENTS[1], json!({"context":"new"})));
        assert!(contexts.ids().is_empty());
        assert!(!contexts.apply(&event(
            CORE_EVENTS[0],
            json!({"context":"new","parent":null})
        )));
    }
}
