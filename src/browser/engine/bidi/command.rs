use super::{ContextId, event::CORE_EVENTS};
use crate::browser::control::Effect;
use serde_json::{Value, json};

/// Closed transport-development commands. Page programs/input and DevTools
/// extensions require their own typed contracts in later stages.
#[derive(Clone, Debug)]
pub(super) enum Command {
    Status,
    NewSession,
    SubscribeCore,
    Tree,
    CreateTab,
    CloseContext(ContextId),
    #[cfg(test)]
    TestPayload {
        effect: Effect,
        text: String,
    },
}

impl Command {
    pub fn belongs_to(&self, scope: u64) -> bool {
        match self {
            Self::CloseContext(context) => context.belongs_to(scope),
            _ => true,
        }
    }
    pub fn encode(&self) -> (&'static str, Value, Effect) {
        match self {
            Self::Status => ("session.status", json!({}), Effect::None),
            Self::NewSession => ("session.new", json!({"capabilities":{}}), Effect::None),
            Self::SubscribeCore => (
                "session.subscribe",
                json!({"events":CORE_EVENTS}),
                Effect::None,
            ),
            Self::Tree => ("browsingContext.getTree", json!({}), Effect::None),
            Self::CreateTab => (
                "browsingContext.create",
                json!({"type":"tab","background":true}),
                Effect::Page,
            ),
            Self::CloseContext(context) => (
                "browsingContext.close",
                json!({"context":context.as_str()}),
                Effect::Page,
            ),
            #[cfg(test)]
            Self::TestPayload { effect, text } => ("test.payload", json!({"text":text}), *effect),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_wire_names_and_effects_are_explicit() {
        let context = ContextId::parse(1, &json!("fixture")).unwrap();
        for (command, name, effect) in [
            (Command::Status, "session.status", Effect::None),
            (Command::NewSession, "session.new", Effect::None),
            (Command::SubscribeCore, "session.subscribe", Effect::None),
            (Command::Tree, "browsingContext.getTree", Effect::None),
            (Command::CreateTab, "browsingContext.create", Effect::Page),
            (
                Command::CloseContext(context),
                "browsingContext.close",
                Effect::Page,
            ),
        ] {
            let (actual, params, classified) = command.encode();
            assert_eq!((actual, classified), (name, effect));
            assert!(params.is_object());
        }
    }
}
