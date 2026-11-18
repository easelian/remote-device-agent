//! Reader-owned binding between one attached target, its top document's
//! default execution context and an authorized diagnostic window.
use super::{console_event, sessions::MAX_SESSION_ID_BYTES};
use crate::browser::semantic::task_events::{Fact, Sink};
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::mpsc;

#[derive(Default)]
struct State {
    parent: Option<String>,
    child: Option<String>,
    context: Option<i64>,
    armed: Option<f64>,
    failed: bool,
    closing: bool,
}

enum Delivery {
    Primary,
    Private,
    Fact(Fact),
    Fault,
}

pub(super) struct Route {
    target: String,
    sink: Sink,
    releases: mpsc::Sender<String>,
    state: Mutex<State>,
}

impl Route {
    pub(super) fn new(target: String, sink: Sink, releases: mpsc::Sender<String>) -> Self {
        Self {
            target,
            sink,
            releases,
            state: Mutex::default(),
        }
    }
    pub(super) fn parent(&self) -> Option<String> {
        self.state.lock().unwrap().parent.clone()
    }
    pub(super) fn child(&self) -> Option<String> {
        self.state.lock().unwrap().child.clone()
    }
    pub(super) fn failed(&self) -> bool {
        self.state.lock().unwrap().failed
    }
    pub(super) fn closing(&self) {
        self.state.lock().unwrap().closing = true;
    }
    pub(super) fn fail(&self) {
        let mut state = self.state.lock().unwrap();
        state.failed = true;
        let invalidate = !state.closing;
        drop(state);
        if invalidate {
            self.sink.invalidate_active();
        }
    }
    pub(super) fn arm(&self, timestamp: f64) -> bool {
        let mut state = self.state.lock().unwrap();
        if !timestamp.is_finite()
            || timestamp <= 0.0
            || state.armed.is_some()
            || state.context.is_none()
            || state.failed
            || state.closing
        {
            return false;
        }
        state.armed = Some(timestamp);
        true
    }

    pub(super) fn consume(&self, event: &Value) -> bool {
        let decision = self.route(&mut self.state.lock().unwrap(), event);
        match decision {
            Delivery::Primary => return false,
            Delivery::Private => {}
            Delivery::Fact(Fact::Exception) if self.sink.wants_exception_locations() => {
                self.sink.push_exception(
                    &event["params"]["exceptionDetails"]["stackTrace"],
                    "scriptId",
                );
            }
            Delivery::Fact(fact) => self.sink.push(fact),
            Delivery::Fault => self.fail(),
        }
        true
    }

    fn route(&self, state: &mut State, event: &Value) -> Delivery {
        use Delivery::*;
        let method = event["method"].as_str().unwrap_or("");
        let params = &event["params"];
        let outer = event.get("sessionId");
        let on_parent = state
            .parent
            .as_ref()
            .is_some_and(|id| outer.and_then(Value::as_str) == Some(id.as_str()));
        let on_child = state
            .child
            .as_ref()
            .is_some_and(|id| outer.and_then(Value::as_str) == Some(id.as_str()));
        let sid = params["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= MAX_SESSION_ID_BYTES);
        if outer.is_none()
            && method == "Target.attachedToTarget"
            && params["targetInfo"]["type"] == "browser"
        {
            return match (state.parent.is_none(), sid) {
                (true, Some(id)) => {
                    state.parent = Some(id.into());
                    Private
                }
                _ => Fault,
            };
        }
        if method == "Target.detachedFromTarget"
            && sid.is_some()
            && (sid == state.parent.as_deref() || sid == state.child.as_deref())
        {
            return if state.closing { Private } else { Fault };
        }
        if on_parent && method == "Target.attachedToTarget" {
            if let Some(id) = sid
                && state.child.is_none()
                && params["targetInfo"]["targetId"] == self.target
                && params["targetInfo"]["type"] == "page"
            {
                state.child = Some(id.into());
                return Private;
            }
            return Fault;
        }
        if !on_child {
            return if on_parent { Private } else { Primary };
        }
        if state.closing || state.failed {
            return Private;
        }
        let context = &params["context"];
        match method {
            "Runtime.executionContextCreated"
                if context["auxData"]["isDefault"] == true
                    && context["auxData"]["frameId"] == self.target =>
            {
                let id = context["id"].as_i64().filter(|&id| id > 0);
                if id.is_none() || (state.context.is_some() && state.context != id) {
                    return Fault;
                }
                state.context = id;
                return Private;
            }
            "Runtime.executionContextsCleared" => {
                state.context = None;
                return if state.armed.is_some() {
                    Fault
                } else {
                    Private
                };
            }
            "Runtime.executionContextDestroyed"
                if state.context.is_some()
                    && params["executionContextId"].as_i64() == state.context =>
            {
                state.context = None;
                return if state.armed.is_some() {
                    Fault
                } else {
                    Private
                };
            }
            _ => {}
        }
        // Reap replayed, non-root and unprojected events too. The channel bounds
        // outstanding native IDs. On overflow the owner detaches the whole subtree.
        let Ok(objects) = console_event::objects(method, params) else {
            return Fault;
        };
        for id in objects {
            if self.releases.try_send(id.to_owned()).is_err() {
                return Fault;
            }
        }
        let context = match method {
            "Runtime.consoleAPICalled" => params["executionContextId"].as_i64(),
            "Runtime.exceptionThrown" => params["exceptionDetails"]["executionContextId"].as_i64(),
            _ => return Private,
        };
        if state.context.is_some()
            && state.context == context
            && let Some(armed) = state.armed
            && console_event::timestamp_ms(params).is_some_and(|t| t >= armed)
            && let Some(fact) = console_event::project(method, params)
        {
            Fact(fact)
        } else {
            Private
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::control::Control;
    use serde_json::json;
    use std::sync::Arc;

    fn route(capacity: usize) -> (Route, mpsc::Receiver<String>, Sink) {
        let (tx, rx) = mpsc::channel(capacity);
        let sink = Sink::new(Arc::new(Control::new()), 1, true, true);
        (Route::new("target".into(), sink.clone(), tx), rx, sink)
    }

    fn location_route() -> (Route, Sink) {
        let (tx, _rx) = mpsc::channel(8);
        let sink = Sink::new_with_tracking(Arc::new(Control::new()), 1, true, true, false, true);
        (Route::new("target".into(), sink.clone(), tx), sink)
    }

    fn parent() -> Value {
        json!({"method":"Target.attachedToTarget","params":{
            "sessionId":"parent","targetInfo":{"type":"browser","targetId":"browser"}}})
    }

    fn child() -> Value {
        json!({"method":"Target.attachedToTarget","sessionId":"parent","params":{
            "sessionId":"child","targetInfo":{"type":"page","targetId":"target"}}})
    }

    fn console(ts: f64, id: Option<&str>) -> Value {
        let mut params = json!({"executionContextId":1,"timestamp":ts,"type":"error","args":[]});
        if let Some(id) = id {
            params["args"] = json!([{"objectId":id}]);
        }
        json!({"method":"Runtime.consoleAPICalled","sessionId":"child","params":params})
    }

    #[test]
    fn routes_only_the_dedicated_browser_page_pair() {
        let (route, _rx, _sink) = route(8);
        assert!(route.consume(&parent()));
        assert!(
            !route.consume(&json!({"method":"Target.attachedToTarget","params":{
            "sessionId":"other","targetInfo":{"type":"page","targetId":"other"}}}))
        );
        assert!(route.consume(&child()));
        assert!(!route.consume(
            &json!({"method":"Runtime.consoleAPICalled","sessionId":"other","params":{
            "timestamp":1.0,"type":"error","args":[]}})
        ));
        assert!(route.consume(&console(1.0, None)));
        assert_eq!(route.parent().as_deref(), Some("parent"));
        assert_eq!(route.child().as_deref(), Some("child"));
    }

    #[test]
    fn unexpected_child_attach_is_private_and_fails_route() {
        let (route, _rx, sink) = route(8);
        route.consume(&parent());
        assert!(route.consume(
            &json!({"method":"Target.attachedToTarget","sessionId":"parent",
            "params":{"sessionId":"unexpected","targetInfo":{"type":"page","targetId":"other"}}})
        ));
        assert!(route.failed());
        assert!(sink.page(0, None, 8).is_err());
    }

    #[test]
    fn old_metadata_and_timestamp_are_ignored_but_new_timestamp_projects() {
        let (route, _rx, sink) = route(8);
        route.consume(&parent());
        route.consume(&child());
        route.consume(&json!({"sessionId":"child","method":"Runtime.executionContextCreated","params":{"context":{"id":1,"auxData":{"isDefault":true,"frameId":"target"}}}}));
        route.arm(10.0);
        route.consume(
            &json!({"method":"Runtime.executionContextCreated","sessionId":"child",
            "params":{"timestamp":1.0}}),
        );
        route.consume(&console(9.0, None));
        assert!(
            sink.page(0, None, 8).unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        route.consume(&console(10.0, None));
        assert_eq!(
            sink.page(0, None, 8).unwrap()["events"][0]["kind"],
            "console"
        );
    }

    #[test]
    fn opted_in_exception_stack_uses_one_based_cdp_positions_and_bounds_frames() {
        let (route, sink) = location_route();
        route.consume(&parent());
        route.consume(&child());
        route.consume(&json!({"sessionId":"child","method":"Runtime.executionContextCreated","params":{"context":{"id":1,"auxData":{"isDefault":true,"frameId":"target"}}}}));
        route.arm(1.0);
        assert!(route.consume(
            &json!({"method":"Runtime.exceptionThrown","sessionId":"child","params":{
                "timestamp":1.0,"exceptionDetails":{"executionContextId":1,"stackTrace":{
                    "callFrames":[
                        {"scriptId":"script","lineNumber":0,"columnNumber":0},
                        {"scriptId":"script","lineNumber":4,"columnNumber":8},
                        {"scriptId":"script","lineNumber":9,"columnNumber":2},
                        {"scriptId":"script","lineNumber":12,"columnNumber":3}
                    ]
                }}
            }})
        ));
        let location = sink.exception_location(1).unwrap();
        assert_eq!(location["frames"][0]["line"], 1);
        assert_eq!(location["frames"][0]["column"], 1);
        assert_eq!(location["frames"].as_array().unwrap().len(), 3);
        assert_eq!(location["truncated"], true);
    }

    #[test]
    fn overflow_or_malformed_runtime_invalidates_without_payload_leak() {
        let (route, mut rx, sink) = route(1);
        route.consume(&parent());
        route.consume(&child());
        route.consume(&console(1.0, Some("one")));
        route.consume(&console(2.0, Some("two")));
        assert!(route.failed());
        assert!(sink.page(0, None, 8).is_err());
        assert_eq!(rx.try_recv().unwrap(), "one");
        let malformed = json!({"method":"Runtime.consoleAPICalled","sessionId":"child",
            "params":{"timestamp":3.0,"type":"error","args":[{"objectId":42}]}});
        assert!(route.consume(&malformed));
    }

    #[test]
    fn foreign_and_failed_routes_swallow_without_leak_or_projection() {
        let (route, mut rx, sink) = route(8);
        route.consume(&parent());
        route.consume(&child());
        route.consume(&json!({"sessionId":"child","method":"Runtime.executionContextCreated","params":{"context":{"id":1,"auxData":{"isDefault":true,"frameId":"target"}}}}));
        route.arm(1.0);
        route.consume(
            &json!({"method":"Runtime.executionContextsCleared","sessionId":"child",
            "params":{}}),
        );
        assert!(route.failed());
        assert!(route.consume(&console(2.0, Some("secret"))));
        assert!(rx.try_recv().is_err());
        assert!(sink.page(0, None, 8).is_err());
        assert!(!route.consume(
            &json!({"method":"Runtime.consoleAPICalled","sessionId":"foreign",
            "params":{"timestamp":2.0,"type":"error","args":[]}})
        ));
    }

    #[test]
    fn cleanup_shield_keeps_old_capture_readable() {
        let (route, _rx, sink) = route(2);
        route.consume(&parent());
        route.consume(&child());
        route.consume(&json!({"sessionId":"child","method":"Runtime.executionContextCreated","params":{"context":{"id":1,"auxData":{"isDefault":true,"frameId":"target"}}}}));
        route.arm(1.0);
        route.consume(&console(1.0, None));
        route.closing();
        route
            .consume(&json!({"method":"Target.detachedFromTarget","params":{"sessionId":"child"}}));
        assert!(!route.failed());
        assert_eq!(
            sink.page(0, None, 8).unwrap()["events"][0]["kind"],
            "console"
        );
    }
}
