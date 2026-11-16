use super::{ContextId, event::CORE_EVENTS};
use crate::browser::control::Effect;
use crate::browser::semantic::{
    program::{self, Program},
    types::Marker,
};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub(super) enum Action {
    Key { value: String, down: bool },
    MoveTo { node: String },
    Move { x: f64, y: f64 },
    Wheel { x: f64, y: f64, delta: f64 },
    Button { down: bool },
    Text(String),
    SelectAll,
}

/// Closed transport-development commands. Page programs/input and DevTools
/// extensions require their own typed contracts in later stages.
#[derive(Clone, Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux owns native page sessions.
pub(super) enum Command {
    Status,
    NewSession,
    SubscribeCore,
    Tree,
    CreateTab,
    CloseContext(ContextId),
    Activate(ContextId),
    Screenshot(ContextId),
    Dialog {
        context: ContextId,
        accept: bool,
    },
    SubscribePage,
    Intercept,
    Program {
        context: ContextId,
        program: Program,
    },
    Document(ContextId),
    Node {
        context: ContextId,
        marker: Marker,
        node: u64,
    },
    FrameOwner {
        context: ContextId,
        child: ContextId,
    },
    FrameGeometry {
        context: ContextId,
        node: String,
        geometry: crate::browser::semantic::Geometry,
    },
    FrameFocused {
        context: ContextId,
        node: String,
    },
    FramePoint {
        context: ContextId,
        node: String,
        geometry: crate::browser::semantic::Geometry,
    },
    Navigate {
        context: ContextId,
        url: String,
    },
    Action {
        context: ContextId,
        action: Action,
    },
    ReleaseInput(ContextId),
    Decide {
        request: String,
        allow: bool,
    },
    #[cfg(test)]
    Fixture {
        context: ContextId,
        expression: String,
    },
    #[cfg(test)]
    TestPayload {
        effect: Effect,
        text: String,
    },
}

impl Command {
    pub fn belongs_to(&self, scope: u64) -> bool {
        match self {
            Self::CloseContext(context)
            | Self::Activate(context)
            | Self::Screenshot(context)
            | Self::Dialog { context, .. }
            | Self::Document(context)
            | Self::ReleaseInput(context)
            | Self::Program { context, .. }
            | Self::Node { context, .. }
            | Self::Navigate { context, .. }
            | Self::Action { context, .. } => context.belongs_to(scope),
            Self::FrameOwner { context, child } => {
                context.belongs_to(scope) && child.belongs_to(scope)
            }
            Self::FrameGeometry { context, .. }
            | Self::FrameFocused { context, .. }
            | Self::FramePoint { context, .. } => context.belongs_to(scope),
            #[cfg(test)]
            Self::Fixture { context, .. } => context.belongs_to(scope),
            _ => true,
        }
    }
    pub fn encode(&self) -> (&'static str, Value, Effect) {
        match self {
            Self::Status => ("session.status", json!({}), Effect::None),
            Self::NewSession => (
                "session.new",
                json!({"capabilities":{"alwaysMatch":{"unhandledPromptBehavior":{"default":"ignore"}}}}),
                Effect::None,
            ),
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
            Self::Activate(context) => (
                "browsingContext.activate",
                json!({"context":context.as_str()}),
                Effect::Page,
            ),
            Self::Screenshot(context) => (
                "browsingContext.captureScreenshot",
                json!({"context":context.as_str(),"origin":"viewport","format":{"type":"image/jpeg","quality":0.65}}),
                Effect::None,
            ),
            Self::Dialog { context, accept } => (
                "browsingContext.handleUserPrompt",
                json!({"context":context.as_str(),"accept":accept}),
                Effect::Page,
            ),
            Self::SubscribePage => (
                "session.subscribe",
                json!({"events":super::event::PAGE_EVENTS}),
                Effect::None,
            ),
            Self::Intercept => (
                "network.addIntercept",
                json!({"phases":["beforeRequestSent"]}),
                Effect::None,
            ),
            Self::Program { context, program } => {
                let (operation, args) = program.encode();
                (
                    "script.callFunction",
                    json!({"target":{"context":context.as_str(),"sandbox":"rdh-semantic-dom-v1"},"functionDeclaration":format!("function(op,args){{return ({})(op,JSON.parse(args));}}",program::SOURCE),"arguments":[{"type":"string","value":operation},{"type":"string","value":args.to_string()}],"awaitPromise":false,"resultOwnership":"none"}),
                    program.effect(),
                )
            }
            Self::Document(context) => (
                "script.evaluate",
                json!({"target":{"context":context.as_str(),"sandbox":"rdh-semantic-dom-v1"},"expression":"document","awaitPromise":false,"resultOwnership":"none","serializationOptions":{"maxDomDepth":0,"maxObjectDepth":0}}),
                Effect::None,
            ),
            Self::Node {
                context,
                marker,
                node,
            } => (
                "script.callFunction",
                json!({"target":{"context":context.as_str(),"sandbox":"rdh-semantic-dom-v1"},"functionDeclaration":program::NATIVE_NODE,"arguments":[{"type":"string","value":json!({"realm":marker.realm,"restoration":marker.restoration,"node":node}).to_string()}],"awaitPromise":false,"resultOwnership":"none","serializationOptions":{"maxDomDepth":0,"maxObjectDepth":0}}),
                Effect::None,
            ),
            Self::Navigate { context, url } => (
                "browsingContext.navigate",
                json!({"context":context.as_str(),"url":url,"wait":"complete"}),
                Effect::Page,
            ),
            Self::FrameOwner { context, child } => (
                "browsingContext.locateNodes",
                json!({"context":context.as_str(),"locator":{"type":"context","value":{"context":child.as_str()}},"maxNodeCount":2,"serializationOptions":{"maxDomDepth":0,"maxObjectDepth":0}}),
                Effect::None,
            ),
            Self::FrameGeometry {
                context,
                node,
                geometry,
            } => (
                "script.callFunction",
                json!({"target":{"context":context.as_str(),"sandbox":"rdh-semantic-dom-v1"},"functionDeclaration":format!("function(args,node){{return ({})(\"native_frame\",JSON.parse(args),node);}}",program::SOURCE),"arguments":[{"type":"string","value":serde_json::to_string(geometry).unwrap()},{"sharedId":node}],"awaitPromise":false,"resultOwnership":"none"}),
                Effect::None,
            ),
            Self::FrameFocused { context, node } => (
                "script.callFunction",
                json!({"target":{"context":context.as_str(),"sandbox":"rdh-semantic-dom-v1"},"functionDeclaration":format!("function(node){{return ({})(\"native_focused_frame\",null,node);}}",program::SOURCE),"arguments":[{"sharedId":node}],"awaitPromise":false,"resultOwnership":"none"}),
                Effect::None,
            ),
            Self::FramePoint {
                context,
                node,
                geometry,
            } => (
                "script.callFunction",
                json!({"target":{"context":context.as_str(),"sandbox":"rdh-semantic-dom-v1"},"functionDeclaration":format!("function(args,node){{return ({})(\"native_frame_point\",JSON.parse(args),node);}}",program::SOURCE),"arguments":[{"type":"string","value":serde_json::to_string(geometry).unwrap()},{"sharedId":node}],"awaitPromise":false,"resultOwnership":"none"}),
                Effect::None,
            ),
            Self::Action { context, action } => {
                let (source, kind, actions) = match action {
                    Action::Key { value, down } => (
                        "rdh-owned-key",
                        "key",
                        vec![json!({"type":if *down{"keyDown"}else{"keyUp"},"value":value})],
                    ),
                    Action::Move { x, y } => (
                        "rdh-owned-pointer",
                        "pointer",
                        vec![
                            json!({"type":"pointerMove","x":x.round() as i64,"y":y.round() as i64,"origin":"viewport"}),
                        ],
                    ),
                    Action::Wheel { x, y, delta } => (
                        "rdh-owned-wheel",
                        "wheel",
                        vec![
                            json!({"type":"scroll","x":x.round() as i64,"y":y.round() as i64,"origin":"viewport","deltaX":0,"deltaY":delta.round() as i64}),
                        ],
                    ),
                    Action::MoveTo { node } => (
                        "rdh-owned-pointer",
                        "pointer",
                        vec![
                            json!({"type":"pointerMove","x":0,"y":0,"origin":{"type":"element","element":{"sharedId":node}}}),
                        ],
                    ),
                    Action::Button { down } => (
                        "rdh-owned-pointer",
                        "pointer",
                        vec![json!({"type":if *down{"pointerDown"}else{"pointerUp"},"button":0})],
                    ),
                    Action::Text(text) => (
                        "rdh-owned-key",
                        "key",
                        text.chars()
                            .flat_map(|c| {
                                let c = if c == '\n' {
                                    "\u{e007}".into()
                                } else {
                                    c.to_string()
                                };
                                [
                                    json!({"type":"keyDown","value":c}),
                                    json!({"type":"keyUp","value":c}),
                                ]
                            })
                            .collect(),
                    ),
                    Action::SelectAll => {
                        let modifier = if cfg!(target_os = "macos") {
                            "\u{e03d}"
                        } else {
                            "\u{e009}"
                        };
                        (
                            "rdh-owned-key",
                            "key",
                            vec![
                                json!({"type":"keyDown","value":modifier}),
                                json!({"type":"keyDown","value":"a"}),
                                json!({"type":"keyUp","value":"a"}),
                                json!({"type":"keyUp","value":modifier}),
                            ],
                        )
                    }
                };
                (
                    "input.performActions",
                    json!({"context":context.as_str(),"actions":[{"type":kind,"id":source,"actions":actions}]}),
                    Effect::Page,
                )
            }
            Self::ReleaseInput(context) => (
                "input.releaseActions",
                json!({"context":context.as_str()}),
                Effect::Page,
            ),
            Self::Decide { request, allow } => (
                if *allow {
                    "network.continueRequest"
                } else {
                    "network.failRequest"
                },
                json!({"request":request}),
                Effect::None,
            ),
            #[cfg(test)]
            Self::Fixture {
                context,
                expression,
            } => (
                "script.evaluate",
                json!({"target":{"context":context.as_str()},"expression":expression,"awaitPromise":true,"resultOwnership":"none"}),
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
