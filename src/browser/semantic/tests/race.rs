use super::{
    observation::{self, Change, Harness, Scene},
    *,
};

pub(super) async fn assert_dispatch_race<P: Harness>(port: P) -> Value {
    port.scene(Scene::Records).await.unwrap();
    let page = Page::new(port.clone());
    let reference = observation::reference(&observation::read(&page).await, "a");
    let old = reference.clone();
    port.clear().await.unwrap();
    let (ready, release) = port.coordination().hold_click_dispatch();
    let owner = page.clone();
    let action = tokio::spawn(async move { owner.act(reference, Action::Click, deadline()).await });
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    // Native hit testing has finished. The browser has not received the
    // mouse press, and the fixture replaces the DOM node at the same point.
    port.change(Change::ReplaceAtDispatch).await.unwrap();
    release.send(()).unwrap();
    let acknowledgement = action.await.unwrap().unwrap();
    page.settled(deadline()).await.unwrap();
    let evidence = port.evidence("replacement").await.unwrap();
    let clicks = evidence["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "click")
        .collect::<Vec<_>>();
    assert_eq!(clicks.len(), 1);
    assert_eq!(clicks[0]["id"], "replacement");
    assert_eq!(clicks[0]["trusted"], true);
    assert!(acknowledgement.input_acknowledged);
    assert!(!acknowledgement.business_result_verified);
    let error = page.act(old, Action::Click, deadline()).await.unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_stale_reference", "not_sent")
    );
    let after = port.evidence("replacement").await.unwrap();
    assert_eq!(after["events"], evidence["events"]);
    json!({"phase":"after_last_native_check_before_mouse_press","actual_target":"replacement","atomic_target_guarantee":false,"old_reference_rejected":true,"acknowledgement":acknowledgement,"events":evidence["events"]})
}

#[tokio::test]
async fn fake_exposes_the_same_dispatch_gap_and_rejects_the_retired_reference() {
    assert_dispatch_race(Fake::new()).await;
}
