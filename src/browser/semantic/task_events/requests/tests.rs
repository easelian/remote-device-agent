use super::*;
use crate::browser::control::Control;
use crate::browser::semantic::task_events::Sink;
use std::sync::Arc;

fn start(ledger: &mut Ledger, native: &str, hop: Option<u32>, method: Method, redirect: bool) {
    ledger.update(
        native,
        RequestUpdate::Start {
            method,
            endpoint: None,
            context: None,
            hop,
            redirect,
        },
    );
}
fn response(ledger: &mut Ledger, native: &str, hop: Option<u32>, status: u16) {
    ledger.update(native, RequestUpdate::Response { status, hop });
}

#[test]
fn thirty_two_slots_keep_contiguous_window_local_ids() {
    let mut ledger = Ledger::default();
    for n in 0..33 {
        start(&mut ledger, &format!("n{n}"), None, Method::Get, false);
    }
    let value = ledger.snapshot("active");
    let rows = value["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 32);
    assert_eq!(rows[0]["request_id"], 2);
    assert_eq!(rows[31]["request_id"], 33);
    assert_eq!(value["evicted_count"], 1);
}

#[test]
fn terminal_without_retained_start_gets_a_window_id() {
    let mut ledger = Ledger::default();
    response(&mut ledger, "late", None, 204);
    let row = &ledger.snapshot("active")["requests"][0];
    assert_eq!(row["request_id"], 1);
    assert_eq!(row["start_observed"], false);
    assert_eq!(row["phase"], "response");
    assert!(row.get("method").is_none());
}

#[test]
fn malformed_native_ids_are_only_untracked() {
    let mut ledger = Ledger::default();
    response(&mut ledger, "", None, 200);
    response(&mut ledger, &"x".repeat(257), None, 200);
    assert_eq!(ledger.snapshot("active")["untracked_count"], 2);
    assert!(
        ledger.snapshot("active")["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn redirect_predecessor_eviction_reports_gap() {
    let mut ledger = Ledger::default();
    start(&mut ledger, "chain", Some(0), Method::Get, false);
    for n in 0..32 {
        start(&mut ledger, &format!("filler{n}"), None, Method::Get, false);
    }
    start(&mut ledger, "chain", Some(1), Method::Get, true);
    let row = &ledger.snapshot("active")["requests"][31];
    assert_eq!(row["phase"], "awaiting_response");
    assert_eq!(row["redirect_gap"], true);
    assert!(row.get("redirected_from").is_none());
}

#[test]
fn cdp_reuses_native_id_and_terminal_updates_one_row() {
    let mut ledger = Ledger::default();
    start(&mut ledger, "cdp", None, Method::Get, false);
    response(&mut ledger, "cdp", None, 200);
    ledger.update(
        "cdp",
        RequestUpdate::Failed {
            reason: FailureKind::Timeout,
            hop: None,
        },
    );
    let value = ledger.snapshot("active");
    let rows = value["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["phase"], "failed");
    assert!(rows[0].get("status").is_none());
}

#[test]
fn firefox_hops_are_exactly_correlated() {
    let mut ledger = Ledger::default();
    start(&mut ledger, "bidi", Some(1), Method::Get, false);
    start(&mut ledger, "bidi", Some(2), Method::Get, false);
    response(&mut ledger, "bidi", Some(1), 200);
    let value = ledger.snapshot("active");
    let rows = value["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["phase"], "response");
    assert_eq!(rows[1]["phase"], "awaiting_response");
}

#[test]
fn unknown_method_is_observed_as_other() {
    let mut ledger = Ledger::default();
    start(&mut ledger, "unknown", None, Method::Other, false);
    let row = &ledger.snapshot("active")["requests"][0];
    assert_eq!(row["start_observed"], true);
    assert_eq!(row["method"], "OTHER");
}

#[test]
fn sink_stop_and_control_takeover_freeze_progress() {
    let control = Arc::new(Control::new());
    let sink = Sink::new_with_progress(control.clone(), 1, false, true, true);
    sink.stop();
    sink.progress(
        "stopped",
        RequestUpdate::Response {
            status: 200,
            hop: None,
        },
    );
    assert!(
        sink.requests().unwrap()["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let sink = Sink::new_with_progress(control.clone(), 1, false, true, true);
    control.change(true);
    sink.progress(
        "human",
        RequestUpdate::Response {
            status: 200,
            hop: None,
        },
    );
    assert!(sink.requests().is_err());
}

#[test]
fn endpoint_rejects_private_parts_and_keeps_no_public_url() {
    let endpoint =
        RequestEndpoint::from_url("https://Example.test/orders/B42?view=full#fragment").unwrap();
    let same =
        RequestEndpoint::from_url("https://example.test/orders/B42?view=full#other").unwrap();
    assert_eq!(endpoint, same);
    assert!(RequestEndpoint::from_url("https://user:pass@example.test/orders").is_none());
}
#[test]
fn submit_response_requires_unique_observed_request() {
    let endpoint = RequestEndpoint::from_url("https://example.test/orders/B42").unwrap();
    let mut ledger = Ledger::default();
    ledger.update(
        "a",
        RequestUpdate::Start {
            method: Method::Post,
            endpoint: Some(endpoint.clone()),
            context: None,
            hop: None,
            redirect: false,
        },
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::Pending
    );
    ledger.update(
        "a",
        RequestUpdate::Response {
            status: 422,
            hop: None,
        },
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::Response {
            request_id: 1,
            status: 422
        }
    );
    ledger.update(
        "b",
        RequestUpdate::Start {
            method: Method::Post,
            endpoint: Some(endpoint.clone()),
            context: None,
            hop: None,
            redirect: false,
        },
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::Undetermined
    );
}
#[test]
fn missing_endpoint_or_failed_request_is_undetermined() {
    let endpoint = RequestEndpoint::from_url("https://example.test/orders/B42").unwrap();
    let mut ledger = Ledger::default();
    ledger.update(
        "a",
        RequestUpdate::Start {
            method: Method::Post,
            endpoint: None,
            context: None,
            hop: None,
            redirect: false,
        },
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::Undetermined
    );
}

#[test]
fn child_context_same_endpoint_is_not_top_level_evidence() {
    let endpoint = RequestEndpoint::from_url("https://example.test/orders/B42").unwrap();
    let mut ledger = Ledger::default();
    ledger.update(
        "top",
        RequestUpdate::Start {
            method: Method::Post,
            endpoint: Some(endpoint.clone()),
            context: Some("top-frame".into()),
            hop: None,
            redirect: false,
        },
    );
    ledger.update(
        "child",
        RequestUpdate::Start {
            method: Method::Post,
            endpoint: Some(endpoint.clone()),
            context: Some("child-frame".into()),
            hop: None,
            redirect: false,
        },
    );
    ledger.update(
        "child",
        RequestUpdate::Response {
            status: 200,
            hop: None,
        },
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, Some("top-frame"), true),
        SubmitResponse::Pending
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, Some("child-frame"), true),
        SubmitResponse::Response {
            request_id: 2,
            status: 200
        }
    );
    // Omitting the native context is deliberately conservative: it must not
    // select either frame, even when only one has completed.
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::NotObserved
    );
}

#[test]
fn action_epoch_rejects_completion_without_observed_start() {
    let mut ledger = Ledger::for_action_epoch(1);
    ledger.update(
        "old",
        RequestUpdate::Response {
            status: 422,
            hop: None,
        },
    );
    let endpoint = RequestEndpoint::from_url("https://example.test/orders/B42").unwrap();
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::NotObserved
    );
    assert!(
        ledger.snapshot("active")["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn submit_dispatch_excludes_old_starts_but_preserves_public_request_ids() {
    let endpoint = RequestEndpoint::from_url("https://example.test/orders/B42").unwrap();
    let mut ledger = Ledger::default();
    ledger.update(
        "old",
        RequestUpdate::Start {
            method: Method::Post,
            endpoint: Some(endpoint.clone()),
            context: None,
            hop: None,
            redirect: false,
        },
    );
    ledger.submit_dispatch();
    ledger.update(
        "old",
        RequestUpdate::Response {
            status: 422,
            hop: None,
        },
    );
    assert_eq!(
        ledger.submit_response(&endpoint, Method::Post, None, true),
        SubmitResponse::NotObserved
    );
    assert_eq!(ledger.snapshot("active")["requests"][0]["request_id"], 1);
    assert_eq!(ledger.snapshot("active")["requests"][0]["status"], 422);
}

#[test]
fn incomplete_or_ended_submit_windows_do_not_attribute_a_response() {
    let endpoint = RequestEndpoint::from_url("https://example.test/orders/B42").unwrap();
    let mut ledger = Ledger::for_action_epoch(1);
    let start = || RequestUpdate::Start {
        method: Method::Post,
        endpoint: Some(endpoint.clone()),
        context: Some("root".into()),
        hop: None,
        redirect: false,
    };
    let result = |ledger: &Ledger, active| {
        ledger.submit_response(&endpoint, Method::Post, Some("root"), active)
    };
    ledger.update("first", start());
    ledger.update(
        "first",
        RequestUpdate::Response {
            status: 422,
            hop: None,
        },
    );
    assert_eq!(result(&ledger, false), SubmitResponse::Undetermined);
    ledger.update(
        "first",
        RequestUpdate::Redirect {
            status: 303,
            hop: None,
        },
    );
    assert_eq!(result(&ledger, true), SubmitResponse::Undetermined);
    let mut ledger = Ledger::for_action_epoch(1);
    for i in 0..=LIMIT {
        ledger.update(&i.to_string(), start());
    }
    assert!(ledger.evicted > 0);
    assert_eq!(result(&ledger, true), SubmitResponse::Undetermined);
}
