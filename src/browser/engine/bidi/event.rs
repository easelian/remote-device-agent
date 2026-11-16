use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) const CORE_EVENTS: [&str; 2] = [
    "browsingContext.contextCreated",
    "browsingContext.contextDestroyed",
];
pub(super) const MAX_CONTEXTS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
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
}
impl Event {
    pub fn parse(scope: u64, method: &str, params: &Value) -> Option<Self> {
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
            _ => None,
        }
    }
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
