use super::{
    observation::{self, Harness, Scene},
    *,
};

pub(super) async fn assert_controls<P: Harness>(port: P) -> Value {
    let page = Page::new(port.clone());
    port.scene(Scene::Controls).await.unwrap();
    let observed = observation::read(&page).await;
    let secret = observed
        .rows
        .iter()
        .find(|row| row.facts.id == "secret")
        .unwrap();
    assert!(secret.facts.sensitive);
    assert!(secret.facts.value.is_none());
    assert!(
        !serde_json::to_string(&observed)
            .unwrap()
            .contains("never-observe-this")
    );
    assert!(
        observed
            .rows
            .iter()
            .find(|row| row.facts.id == "disabled")
            .is_none_or(|row| row.reference.is_none())
    );
    for id in ["secret", "readonly", "inferred"] {
        let target = observation::reference(&observed, id);
        let error = page
            .act(target, Action::ReplaceText("disallowed".into()), deadline())
            .await
            .unwrap_err();
        // A secret is for a person to type, and the caller is told so.
        let code = if id == "secret" {
            "browser_sensitive_input"
        } else {
            "browser_not_actionable"
        };
        assert_eq!((error.code, error.state), (code, "not_sent"));
    }
    let outer = observation::reference(&observed, "outer");
    let error = page
        .act(outer, Action::Click, deadline())
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_not_actionable", "not_sent")
    );
    for value in ["missing", "disabled"] {
        let select = observation::reference(&observed, "select");
        let error = page
            .act(select, Action::SetFormValue(value.into()), deadline())
            .await
            .unwrap_err();
        assert_eq!(
            (error.code, error.state),
            ("browser_invalid_params", "not_sent")
        );
    }
    assert!(
        port.evidence("keys").await.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut forms = Vec::new();
    for (id, value) in [
        ("select", "b"),
        ("date", "2026-11-16"),
        ("color", "#123abc"),
        ("range", "73"),
    ] {
        let target = observation::reference(&observation::read(&page).await, id);
        port.clear().await.unwrap();
        let ack = page
            .act(target, Action::SetFormValue(value.into()), deadline())
            .await
            .unwrap();
        assert!(ack.changed);
        assert_eq!(ack.path, "synthetic_form");
        let evidence = port.evidence(id).await.unwrap();
        assert_eq!(evidence["value"], value);
        let events = evidence["events"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "input"
                    && event["id"] == id
                    && event["trusted"] == false)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "change"
                    && event["id"] == id
                    && event["trusted"] == false)
                .count(),
            1
        );
        forms.push(json!({"id":id,"value":value,"events":events}));
        let target = observation::reference(&observation::read(&page).await, id);
        port.clear().await.unwrap();
        assert!(
            !page
                .act(target, Action::SetFormValue(value.into()), deadline())
                .await
                .unwrap()
                .changed
        );
        assert!(
            port.evidence(id).await.unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    for expected in [false, true, true, false] {
        let observed = observation::read(&page).await;
        let target = observation::reference(&observed, "check");
        let before = observed
            .rows
            .iter()
            .find(|row| row.facts.id == "check")
            .unwrap()
            .facts
            .checked;
        port.clear().await.unwrap();
        let ack = page
            .act(target, Action::Check(expected), deadline())
            .await
            .unwrap();
        assert_eq!(ack.changed, before != expected);
        let after = observation::read(&page).await;
        assert_eq!(
            after
                .rows
                .iter()
                .find(|row| row.facts.id == "check")
                .unwrap()
                .facts
                .checked,
            expected
        );
        if !ack.changed {
            assert!(
                port.evidence("check").await.unwrap()["events"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }
    let mut keys = Vec::new();
    for key in [
        Key::Enter,
        Key::Tab,
        Key::Escape,
        Key::Backspace,
        Key::ArrowDown,
        Key::ArrowUp,
        Key::ArrowLeft,
        Key::ArrowRight,
    ] {
        let target = observation::reference(&observation::read(&page).await, "keys");
        port.clear().await.unwrap();
        page.act(target, Action::Press(key), deadline())
            .await
            .unwrap();
        let evidence = port.evidence("keys").await.unwrap();
        let name = format!("{key:?}");
        let events = evidence["events"].as_array().unwrap();
        assert!(
            events.iter().any(|event| event["type"] == "keydown"
                && event["key"] == name
                && event["trusted"] == true),
            "missing key {name}: {evidence}"
        );
        assert!(
            events
                .iter()
                .filter(|event| event["type"] == "keydown")
                .all(|event| event["ctrl"] == false && event["meta"] == false)
        );
        keys.push(name);
    }
    for _ in 0..20 {
        let stale = observation::reference(&observation::read(&page).await, "keys");
        let human = page.claim_human(deadline()).await.unwrap();
        assert!(
            page.act(stale, Action::Press(Key::ArrowRight), deadline())
                .await
                .is_err()
        );
        assert!(
            page.observe(Query::default(), 200, 64000, None, deadline())
                .await
                .is_err()
        );
        let viewed = page
            .human_observe(&human, Query::default(), 200, 64000, deadline())
            .await
            .unwrap();
        let target = observation::reference(&viewed, "keys");
        page.human_act(
            &human,
            target.clone(),
            Action::Press(Key::ArrowRight),
            deadline(),
        )
        .await
        .unwrap();
        assert_eq!(
            page.return_to_ai(&human, false, deadline())
                .await
                .unwrap_err()
                .code,
            "browser_disclosure_required"
        );
        page.return_to_ai(&human, true, deadline()).await.unwrap();
        assert!(
            page.human_act(
                &human,
                target.clone(),
                Action::Press(Key::ArrowRight),
                deadline()
            )
            .await
            .is_err()
        );
        assert!(
            page.act(target, Action::Press(Key::ArrowRight), deadline())
                .await
                .is_err()
        );
        page.settled(deadline()).await.unwrap();
        assert_eq!(page.resources().1, 0);
    }
    // A second wrapper of the same adapter must not create another authority.
    let second = Page::new(port.clone());
    let target = observation::reference(&observation::read(&page).await, "keys");
    let epoch = second.pause();
    assert!(
        page.act(target, Action::Press(Key::ArrowRight), deadline())
            .await
            .is_err()
    );
    page.resume(epoch, true, deadline()).await.unwrap();
    let retired = page.claim_human(deadline()).await.unwrap();
    let mut current = second.claim_human(deadline()).await.unwrap();
    assert!(
        page.human_observe(&retired, Query::default(), 200, 64000, deadline())
            .await
            .is_err()
    );
    let viewed = page
        .human_observe(&current, Query::default(), 200, 64000, deadline())
        .await
        .unwrap();
    let target = observation::reference(&viewed, "keys");
    current.expire_for_test();
    port.clear().await.unwrap();
    let error = page
        .human_act(&current, target, Action::Press(Key::ArrowRight), deadline())
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_control_changed", "not_sent")
    );
    assert!(
        port.evidence("keys").await.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(page.return_to_ai(&current, true, deadline()).await.is_err());
    let fresh = page.claim_human(deadline()).await.unwrap();
    page.return_to_ai(&fresh, true, deadline()).await.unwrap();
    json!({"sensitive_readonly_inferred_rejected":true,"nested_control_rejected":true,"forms":forms,"check_verified":true,"keys":keys,"human_handoffs":20,"shared_session_coordination":true,"superseded_human_permit_rejected":true,"expired_human_permit_rejected":true})
}
#[tokio::test]
async fn fake_uses_shared_form_native_key_and_handoff_assertions() {
    assert_controls(Fake::new()).await;
}
