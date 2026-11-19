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

/// Closed native transport and page commands. No caller-supplied methods or scripts.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)] // Public native sessions are validated on Linux x86_64.
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
    DownloadBehavior(Option<std::path::PathBuf>),
    Respond {
        request: String,
        allow: bool,
    },
    Program {
        context: ContextId,
        program: Program,
    },
    VisualTarget {
        context: ContextId,
        x: f64,
        y: f64,
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
    SelectFile {
        context: ContextId,
        node: String,
        path: std::path::PathBuf,
    },
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

fn semantic_call_function(
    context: &ContextId,
    function: String,
    arguments: Value,
    effect: Effect,
) -> (&'static str, Value, Effect) {
    let mut params = json!({
        "target": {
            "context": context.as_str(),
            "sandbox": "rdh-semantic-dom-v1"
        },
        "awaitPromise": false,
        "resultOwnership": "none"
    });
    params["functionDeclaration"] = Value::String(function);
    params["arguments"] = arguments;
    ("script.callFunction", params, effect)
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
            | Self::SelectFile { context, .. }
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
            Self::SelectFile {
                context,
                node,
                path,
            } => (
                "input.setFiles",
                json!({"context":context.as_str(),"element":{"sharedId":node},"files":[path]}),
                Effect::Page,
            ),
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
            Self::DownloadBehavior(path) => (
                "browser.setDownloadBehavior",
                json!({"downloadBehavior":path.as_ref().map_or(json!({"type":"denied"}),|p|json!({"type":"allowed","destinationFolder":p}))}),
                Effect::None,
            ),
            Self::Respond { request, allow } => (
                if *allow {
                    "network.continueResponse"
                } else {
                    "network.failRequest"
                },
                json!({"request":request}),
                Effect::None,
            ),
            Self::Intercept => (
                "network.addIntercept",
                json!({"phases":["beforeRequestSent","responseStarted"]}),
                Effect::None,
            ),
            Self::VisualTarget { context, x, y } => {
                let (method, mut params, effect) = semantic_call_function(
                    context,
                    program::VISUAL_TARGET_NODE.into(),
                    json!([{"type":"number","value":x},{"type":"number","value":y}]),
                    Effect::None,
                );
                params["serializationOptions"] =
                    json!({"maxDomDepth":0,"maxObjectDepth":0,"includeShadowTree":"none"});
                (method, params, effect)
            }
            Self::Program { context, program } => {
                let (operation, args) = program.encode();
                semantic_call_function(
                    context,
                    format!(
                        "function(op,args){{return ({})(op,JSON.parse(args));}}",
                        program.source()
                    ),
                    json!([{"type":"string","value":operation},{"type":"string","value":args.to_string()}]),
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
            } => semantic_call_function(
                context,
                format!(
                    "function(args,node){{return ({})(\"native_frame\",JSON.parse(args),node);}}",
                    program::SOURCE
                ),
                json!([{"type":"string","value":serde_json::to_string(geometry).unwrap()},{"sharedId":node}]),
                Effect::None,
            ),
            Self::FrameFocused { context, node } => semantic_call_function(
                context,
                format!(
                    "function(node){{return ({})(\"native_focused_frame\",null,node);}}",
                    program::SOURCE
                ),
                json!([{"sharedId":node}]),
                Effect::None,
            ),
            Self::FramePoint {
                context,
                node,
                geometry,
            } => semantic_call_function(
                context,
                format!(
                    "function(args,node){{return ({})(\"native_frame_point\",JSON.parse(args),node);}}",
                    program::SOURCE
                ),
                json!([{"type":"string","value":serde_json::to_string(geometry).unwrap()},{"sharedId":node}]),
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
    fn semantic_program_encoder_preserves_call_function_shape() {
        let context = ContextId::parse(1, &json!("fixture")).unwrap();
        let (method, params, effect) = Command::Program {
            context,
            program: Program::Stamp,
        }
        .encode();
        assert_eq!((method, effect), ("script.callFunction", Effect::None));
        assert_eq!(params["target"]["sandbox"], "rdh-semantic-dom-v1");
        assert_eq!(params["awaitPromise"], false);
        assert_eq!(params["resultOwnership"], "none");
        assert_eq!(params["arguments"][0]["value"], "stamp");
        assert_eq!(params["arguments"][1]["value"], "{}");
        assert!(
            params["functionDeclaration"]
                .as_str()
                .unwrap()
                .contains("JSON.parse(args)")
        );
    }

    #[test]
    fn semantic_frame_wrappers_match_original_declarations_and_shape() {
        let context = ContextId::parse(1, &json!("fixture")).unwrap();
        let geometry = crate::browser::semantic::Geometry {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
            visible: true,
            hit: false,
            in_view: true,
        };
        let cases = [
            (
                Command::FrameGeometry {
                    context: context.clone(),
                    node: "n".into(),
                    geometry: geometry.clone(),
                },
                format!(
                    r#"function(args,node){{return ({})("native_frame",JSON.parse(args),node);}}"#,
                    program::SOURCE
                ),
                json!([{"type":"string","value":serde_json::to_string(&geometry).unwrap()},{"sharedId":"n"}]),
            ),
            (
                Command::FrameFocused {
                    context: context.clone(),
                    node: "n".into(),
                },
                format!(
                    r#"function(node){{return ({})("native_focused_frame",null,node);}}"#,
                    program::SOURCE
                ),
                json!([{"sharedId":"n"}]),
            ),
            (
                Command::FramePoint {
                    context,
                    node: "n".into(),
                    geometry: geometry.clone(),
                },
                format!(
                    r#"function(args,node){{return ({})("native_frame_point",JSON.parse(args),node);}}"#,
                    program::SOURCE
                ),
                json!([{"type":"string","value":serde_json::to_string(&geometry).unwrap()},{"sharedId":"n"}]),
            ),
        ];
        for (command, declaration, arguments) in cases {
            let (method, params, effect) = command.encode();
            assert_eq!((method, effect), ("script.callFunction", Effect::None));
            assert_eq!(params["functionDeclaration"], declaration);
            assert_eq!(params["arguments"], arguments);
            assert_eq!(params["target"]["sandbox"], "rdh-semantic-dom-v1");
            assert_eq!(params["awaitPromise"], false);
            assert_eq!(params["resultOwnership"], "none");
        }
    }

    #[test]
    fn move_to_element_dispatches_a_single_pointer_move() {
        let context = ContextId::parse(1, &json!("fixture")).unwrap();
        let (method, params, effect) = Command::Action {
            context,
            action: Action::MoveTo {
                node: "fixture-node".into(),
            },
        }
        .encode();
        assert_eq!((method, effect), ("input.performActions", Effect::Page));
        assert_eq!(
            params["actions"][0]["actions"],
            json!([{
                "type":"pointerMove","x":0,"y":0,"origin":{"type":"element","element":{"sharedId":"fixture-node"}}
            }])
        );
    }

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
