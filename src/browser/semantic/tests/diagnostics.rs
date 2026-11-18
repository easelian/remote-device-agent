use super::*;
use crate::browser::semantic::actionability::{
    self as a, ActionKind as K, Reason as R, Status as S,
};

fn text() -> Facts {
    Fake::new()
        .data
        .lock()
        .unwrap()
        .rows
        .iter()
        .find(|f| f.id == "label")
        .unwrap()
        .clone()
}
fn geometry() -> Geometry {
    Geometry {
        x: 10.,
        y: 10.,
        width: 1280.,
        height: 720.,
        visible: true,
        hit: true,
        in_view: true,
    }
}
fn assessed(facts: &Facts, kind: K) -> a::Diagnostic {
    let g = geometry();
    a::assess(facts, kind, true, Some(&g), Some((facts, &g)))
}

#[test]
fn closed_action_kinds_and_value_independent_preconditions() {
    for (name, kind) in [
        ("click", K::Click),
        ("hover", K::Hover),
        ("press", K::Press),
        ("fill", K::Fill),
        ("check", K::Check),
        ("select", K::Select),
        ("attach_file", K::AttachFile),
    ] {
        assert_eq!(serde_json::from_value::<K>(json!(name)).unwrap(), kind);
    }
    for value in [
        Value::Null,
        json!("navigate"),
        json!("scroll"),
        json!("Fill"),
        json!({"kind":"click"}),
    ] {
        assert!(serde_json::from_value::<K>(value).is_err());
    }
    let plain = text();
    for kind in [K::Click, K::Press, K::Fill] {
        assert_eq!(assessed(&plain, kind).status, S::ReadyNow);
    }
    for kind in [K::Check, K::Select, K::AttachFile] {
        let result = assessed(&plain, kind);
        assert_eq!(result.status, S::Blocked);
        assert_eq!(result.reasons, [R::UnsupportedAction]);
    }
    let mut readonly = plain.clone();
    readonly.readonly = true;
    assert_eq!(assessed(&readonly, K::Fill).reasons, [R::ReadOnly]);
    assert_eq!(assessed(&readonly, K::Click).status, S::ReadyNow);
    let mut sensitive = plain.clone();
    sensitive.sensitive = true;
    sensitive.value = None;
    sensitive.input_type = "password".into();
    assert_eq!(assessed(&sensitive, K::Fill).reasons, [R::SensitiveTarget]);
    assert_eq!(assessed(&sensitive, K::Press).status, S::ReadyNow);
    let mut inferred = plain.clone();
    inferred.kind = "inferred".into();
    assert_eq!(assessed(&inferred, K::Fill).reasons, [R::UnsupportedAction]);
    assert_eq!(assessed(&inferred, K::Click).status, S::ReadyNow);
    let mut checkbox = plain.clone();
    checkbox.role = "checkbox".into();
    assert_eq!(assessed(&checkbox, K::Check).status, S::ReadyNow);
    for input_type in [
        "date",
        "time",
        "datetime-local",
        "month",
        "week",
        "color",
        "range",
    ] {
        let mut control = plain.clone();
        control.input_type = input_type.into();
        assert_eq!(assessed(&control, K::Select).status, S::ReadyNow);
        assert_eq!(assessed(&control, K::Fill).reasons, [R::UnsupportedAction]);
    }
    let mut file = plain.clone();
    file.input_type = "file".into();
    assert_eq!(assessed(&file, K::AttachFile).status, S::ReadyNow);
    assert_eq!(
        a::requirements(&file, K::AttachFile, false),
        [R::UnsupportedAction]
    );
}

#[test]
fn geometric_conditions_never_treat_missing_or_unstable_evidence_as_ready() {
    let facts = text();
    let base = geometry();
    assert_eq!(
        a::assess(&facts, K::Click, true, None, None).reasons,
        [R::UnsupportedGeometry]
    );
    assert_eq!(
        a::assess(&facts, K::Click, true, Some(&base), None).status,
        S::Indeterminate
    );
    let mut covered = base.clone();
    covered.hit = false;
    let result = a::assess(&facts, K::Click, true, Some(&covered), None);
    assert_eq!(
        (result.status, result.reasons),
        (S::Blocked, vec![R::Covered])
    );
    let mut outside = covered.clone();
    outside.in_view = false;
    let result = a::assess(&facts, K::Click, true, Some(&outside), None);
    assert_eq!(
        (result.status, result.reasons),
        (S::NeedsScroll, vec![R::OutsideViewport])
    );
    let mut moved = base.clone();
    moved.x += 0.001;
    let result = a::assess(&facts, K::Click, true, Some(&base), Some((&facts, &moved)));
    assert_eq!(
        (result.status, result.reasons),
        (S::Indeterminate, vec![R::UnstableGeometry])
    );
    assert_eq!(result.conditions.stable, Some(false));
    let result = a::assess(
        &facts,
        K::Click,
        true,
        Some(&base),
        Some((&facts, &covered)),
    );
    assert_eq!(result.status, S::Blocked);
    assert_eq!(result.conditions.hit, Some(false));
    let mut invisible = base.clone();
    invisible.visible = false;
    assert_eq!(
        a::assess(&facts, K::Click, true, Some(&invisible), None).status,
        S::Indeterminate
    );
}

#[test]
fn diagnostic_projection_contains_no_page_text_value_or_native_identity() {
    let mut facts = text();
    facts.id = "private-native-identity".into();
    facts.name = "private-page-name".into();
    facts.value = Some("private-field-value".into());
    let diagnostic = assessed(&facts, K::Fill);
    let value = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(value["diagnostic_version"], 1);
    assert_eq!(value["content_trust"], "untrusted_page");
    let encoded = serde_json::to_string(&value).unwrap();
    for forbidden in [
        "private-native-identity",
        "private-page-name",
        "private-field-value",
        "node",
        "coordinates",
    ] {
        assert!(!encoded.contains(forbidden), "{encoded}");
    }
    let mut maximal = diagnostic;
    maximal.reasons = vec![
        R::ReadOnly,
        R::Covered,
        R::OutsideViewport,
        R::UnstableGeometry,
        R::UnsupportedGeometry,
        R::SensitiveTarget,
        R::UnsupportedAction,
    ];
    assert!(maximal.reasons.len() <= a::MAX_REASONS);
    let mut value = serde_json::to_value(maximal).unwrap();
    value["session_id"] = "s".repeat(128).into();
    value["tab_id"] = "t".repeat(128).into();
    value["control_epoch"] = json!(9_007_199_254_740_991u64);
    assert!(serde_json::to_vec(&value).unwrap().len() < a::MAX_BYTES);
    assert_eq!(a::TIMEOUT, Duration::from_secs(1));
}

#[test]
fn diagnostic_body_checks_received_utf8_bytes_and_reason_count_at_the_limits() {
    let mut value = json!({"reasons":[],"padding":""});
    let overhead = serde_json::to_vec(&value).unwrap().len();
    value["padding"] = "p".repeat(a::MAX_BYTES - overhead).into();
    assert_eq!(serde_json::to_vec(&value).unwrap().len(), a::MAX_BYTES);
    assert!(a::check_size(&value).is_ok());
    value["padding"] = "p".repeat(a::MAX_BYTES - overhead + 1).into();
    assert_eq!(
        a::check_size(&value).unwrap_err().code,
        "browser_semantic_decode_failed"
    );
    value = json!({"reasons":vec!["covered"; a::MAX_REASONS]});
    assert!(a::check_size(&value).is_ok());
    value["reasons"]
        .as_array_mut()
        .unwrap()
        .push(json!("covered"));
    assert!(a::check_size(&value).is_err());
    let mut encoded = json!({"reasons":[],"padding":""});
    let overhead = serde_json::to_vec(&encoded).unwrap().len();
    // Two-byte UTF-8 characters charge their encoding, not scalar count.
    encoded["padding"] = "\u{00e9}".repeat((a::MAX_BYTES - overhead) / 2 + 1).into();
    assert!(a::check_size(&encoded).is_err());
}

async fn reference<P: Port>(page: &Page<P>, id: &str) -> Reference {
    page.observe(Query::default(), 200, 64000, None, deadline())
        .await
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.facts.id == id)
        .unwrap()
        .reference
        .unwrap()
}

async fn assert_read_only<P: observation::Harness>(port: P) -> Value {
    use observation::{Change, Scene};
    let page = Page::new(port.clone());
    port.scene(Scene::Controls).await.unwrap();
    let mut results = Vec::new();
    for (id, kind, status, reason) in [
        ("check", K::Check, S::ReadyNow, None),
        ("select", K::Select, S::ReadyNow, None),
        ("keys", K::Fill, S::ReadyNow, None),
        ("readonly", K::Fill, S::Blocked, Some(R::ReadOnly)),
        ("secret", K::Fill, S::Blocked, Some(R::SensitiveTarget)),
        ("inferred", K::Fill, S::Blocked, Some(R::UnsupportedAction)),
        ("check", K::Select, S::Blocked, Some(R::UnsupportedAction)),
    ] {
        let reference = reference(&page, id).await;
        port.clear().await.unwrap();
        let before = port.evidence(id).await.unwrap();
        let resources = page.resources();
        for _ in 0..3 {
            let result = page
                .diagnose(reference.clone(), kind, deadline())
                .await
                .unwrap();
            assert_eq!(result.status, status, "{id}: {result:?}");
            assert_eq!(
                result.reasons,
                reason.into_iter().collect::<Vec<_>>(),
                "{id}"
            );
            assert_eq!(
                port.evidence(id).await.unwrap(),
                before,
                "diagnostic changed {id}"
            );
            assert_eq!(
                page.resources(),
                resources,
                "diagnostic changed ownership for {id}"
            );
            results.push(json!({"id":id,"kind":kind,"diagnostic":result}));
        }
    }
    let observation = page
        .observe(Query::default(), 200, 64000, None, deadline())
        .await
        .unwrap();
    assert!(
        observation
            .rows
            .iter()
            .any(|row| row.facts.id == "disabled" && row.reference.is_none())
    );
    port.scene(Scene::Records).await.unwrap();
    let below = reference(&page, "c").await;
    let before = port.evidence("c").await.unwrap();
    let result = page
        .diagnose(below.clone(), K::Click, deadline())
        .await
        .unwrap();
    assert_eq!(
        (result.status, result.reasons.clone()),
        (S::NeedsScroll, vec![R::OutsideViewport])
    );
    assert_eq!(port.evidence("c").await.unwrap(), before);
    // A repeated diagnostic cannot silently scroll the target into view.
    assert_eq!(
        page.diagnose(below, K::Click, deadline())
            .await
            .unwrap()
            .status,
        S::NeedsScroll
    );
    results.push(json!({"id":"c","diagnostic":result}));
    let target = reference(&page, "a").await;
    port.change(Change::Occlude).await.unwrap();
    assert_eq!(
        page.diagnose(target.clone(), K::Click, deadline())
            .await
            .unwrap()
            .reasons,
        [R::Covered]
    );
    port.change(Change::Reveal).await.unwrap();
    assert_eq!(
        page.diagnose(target.clone(), K::Click, deadline())
            .await
            .unwrap()
            .status,
        S::ReadyNow
    );
    port.change(Change::Own).await.unwrap();
    assert_eq!(
        page.diagnose(target.clone(), K::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    assert_eq!(
        page.act(target, Action::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    let target = reference(&page, "a").await;
    port.change(Change::Replace).await.unwrap();
    assert_eq!(
        page.diagnose(target, K::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    let target = reference(&page, "a").await;
    page.expire_for_test();
    assert_eq!(
        page.diagnose(target, K::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    assert_eq!(page.resources().1, 0);
    assert_eq!(port.pending(), 0);
    json!({"results":results,"stale_record_rejected":true,"replaced_node_rejected":true,"expiry_preserved":true})
}

#[tokio::test]
async fn fake_diagnostics_are_read_only_and_never_refresh_reference_authority() {
    let port = Fake::new();
    assert_read_only(port.clone()).await;
    assert_eq!(port.effects.load(Ordering::Acquire), 0);
}

trait NativeRead: faults::NativeGate + frames::Harness {
    fn metrics(&self) -> Value;
    fn fixture_read(&self, script: &str) -> impl Future<Output = Result<Value>> + Send;
}
macro_rules! native_read {
    ($ty:ty) => {
        impl NativeRead for $ty {
            fn metrics(&self) -> Value {
                self.test_metrics()
            }
            async fn fixture_read(&self, script: &str) -> Result<Value> {
                self.fixture(script, deadline()).await
            }
        }
    };
}
native_read!(engine::semantic_cdp::Semantic);
native_read!(engine::bidi::page::Semantic);

async fn assert_pending_reads<P: NativeRead>(port: P) -> Value {
    use observation::Scene;
    port.scene(Scene::Controls).await.unwrap();
    let page = Page::new(port.clone());
    let mut cases = Vec::new();
    for mode in ["control", "cancel", "timeout", "lock_deadline"] {
        let reference = reference(&page, "keys").await;
        port.clear().await.unwrap();
        let before = port.evidence("keys").await.unwrap();
        let (reached, release) = port.gate();
        let caller = page.clone();
        let started = Instant::now();
        let target = reference.clone();
        let mut pending = Some(tokio::spawn(async move {
            caller.diagnose(target, K::Fill, deadline()).await
        }));
        tokio::time::timeout(Duration::from_secs(2), reached)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.resources().1, 0, "a diagnostic cannot own input work");
        assert_eq!(page.resources().2, 0, "a read is not an input barrier");
        assert!(port.metrics()["pending"].as_u64().unwrap() >= 1);
        if mode == "cancel" {
            let caller = pending.take().unwrap();
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        } else if mode == "timeout" {
            let result = pending.take().unwrap().await.unwrap().unwrap_err();
            assert!(matches!(
                result.code,
                "browser_timeout_unknown" | "browser_deadline"
            ));
            assert!(started.elapsed() < Duration::from_millis(1500));
        } else if mode == "lock_deadline" {
            let waiting = Instant::now();
            let error = page
                .diagnose(reference, K::Fill, waiting + Duration::from_millis(30))
                .await
                .unwrap_err();
            assert_eq!(error.code, "browser_busy");
            assert!(waiting.elapsed() < Duration::from_millis(300));
        }
        // The read's wire slot remains owned until its actual reply arrives,
        // but a human does not wait for a pure read before acquiring control.
        assert!(port.metrics()["pending"].as_u64().unwrap() >= 1);
        let fence = Instant::now();
        let permit = page
            .claim_human(fence + Duration::from_millis(10))
            .await
            .unwrap();
        assert!(fence.elapsed() < Duration::from_millis(100));
        assert!(page.control().snapshot().1);
        assert!(port.metrics()["pending"].as_u64().unwrap() >= 1);
        release
            .send(())
            .expect("the submitted native reply must remain owned");
        if let Some(caller) = pending {
            let error = caller.await.unwrap().unwrap_err();
            assert!(matches!(
                error.code,
                "browser_control_changed" | "browser_timeout_unknown"
            ));
        }
        let until = deadline();
        while port.metrics()["pending"] != 0 {
            assert!(
                Instant::now() < until,
                "late native read was not settled: {}",
                port.metrics()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(port.evidence("keys").await.unwrap(), before);
        cases.push(json!({"mode":mode,"fence_ms":fence.elapsed().as_secs_f64()*1000.,"metrics":port.metrics(),"page_unchanged":true}));
        page.return_to_ai(&permit, true, deadline()).await.unwrap();
    }
    json!({"cases":cases,"retained_until_reply":true,"no_input_permit":true})
}

async fn assert_native_geometry<P: NativeRead>(port: P, url: &str) -> Value {
    use observation::Scene;
    let page = Page::new(port.clone());
    let mut cases = Vec::new();
    for (change, expected) in [
        (None, S::ReadyNow),
        (Some(frames::Change::Offscreen), S::NeedsScroll),
        (Some(frames::Change::Scale), S::ReadyNow),
        (Some(frames::Change::Cover), S::Blocked),
    ] {
        port.mount(url).await.unwrap();
        let target = reference(&page, "deep").await;
        if let Some(change) = change {
            port.frame_change(change).await.unwrap();
        }
        let result = page.diagnose(target, K::Click, deadline()).await.unwrap();
        assert_eq!(result.status, expected, "{result:?}");
        assert!(port.frame_clicks().await.unwrap().is_empty());
        cases.push(json!({"diagnostic":result}));
    }
    for transform in ["rotate(10deg)", "perspective(600px) rotateY(20deg)"] {
        port.mount(url).await.unwrap();
        let target = reference(&page, "deep").await;
        port.fixture_read(&format!(
            "(document.getElementById('first-frame').style.transform={},true)",
            serde_json::to_string(transform).unwrap()
        ))
        .await
        .unwrap();
        let result = page.diagnose(target, K::Click, deadline()).await.unwrap();
        assert_eq!(result.status, S::Indeterminate, "{transform}: {result:?}");
        assert_eq!(result.reasons, [R::UnsupportedGeometry]);
        assert!(port.frame_clicks().await.unwrap().is_empty());
        cases.push(json!({"transform":transform,"diagnostic":result}));
    }
    for change in [frames::Change::Hide, frames::Change::ChildNavigate] {
        port.mount(url).await.unwrap();
        let target = reference(&page, "deep").await;
        port.frame_change(change).await.unwrap();
        let result = page.diagnose(target, K::Click, deadline()).await;
        if matches!(change, frames::Change::Hide) {
            // The inner node remains eligible in its own document, while the
            // hidden ancestor makes its screen geometry unprovable.
            let result = result.unwrap();
            assert_eq!(result.status, S::Indeterminate);
            assert_eq!(result.reasons, [R::UnsupportedGeometry]);
        } else {
            assert_eq!(result.unwrap_err().code, "browser_stale_reference");
        }
        assert!(port.frame_clicks().await.unwrap().is_empty());
    }
    for property in ["disabled=true", "style.display='none'"] {
        port.scene(Scene::Controls).await.unwrap();
        let target = reference(&page, "keys").await;
        port.fixture_read(&format!(
            "(document.getElementById('keys').{property},true)"
        ))
        .await
        .unwrap();
        assert_eq!(
            page.diagnose(target, K::Fill, deadline())
                .await
                .unwrap_err()
                .code,
            "browser_stale_reference"
        );
    }
    port.scene(Scene::Controls).await.unwrap();
    let target = reference(&page, "keys").await;
    port.fixture_read("(()=>{const e=document.getElementById('keys');e.animate([{transform:'translateX(0px)'},{transform:'translateX(200px)'}],{duration:2000,iterations:Infinity,direction:'alternate'});return true;})()").await.unwrap();
    let result = page.diagnose(target, K::Fill, deadline()).await.unwrap();
    assert_eq!(result.status, S::Indeterminate, "{result:?}");
    assert_eq!(result.reasons, [R::UnstableGeometry]);
    cases.push(json!({"moving":result}));
    json!({"cases":cases,"stale_hidden_disabled_and_child_navigation":true,"frame_clicks":[]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Chrome and evidence directory"]
async fn cdp_diagnostics_share_the_read_only_oracle() {
    let (url, server) = server().await;
    let directory = root("cdp-diagnostics");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut session = engine::semantic_cdp::Session::launch(
        &browser,
        &directory,
        policy::Policy::new(Default::default()),
        deadline(),
    )
    .await
    .unwrap();
    session.navigate(&url, deadline()).await.unwrap();
    record(
        "cdp-diagnostics.json",
        &json!({"ordinary":assert_read_only(session.port.clone()).await,
            "pending":assert_pending_reads(session.port.clone()).await,
            "geometry":assert_native_geometry(session.port.clone(), &url).await}),
    );
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    fs::remove_dir(directory).unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Firefox helper and evidence directory"]
async fn bidi_diagnostics_share_the_read_only_oracle() {
    let (url, server) = server().await;
    let directory = root("bidi-diagnostics");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut session = engine::bidi::page::Session::open(
        &helper,
        &browser,
        &directory,
        policy::Policy::new(Default::default()),
    )
    .await
    .unwrap();
    session.navigate(&url, deadline()).await.unwrap();
    record(
        "bidi-diagnostics.json",
        &json!({"ordinary":assert_read_only(session.port.clone()).await,
            "pending":assert_pending_reads(session.port.clone()).await,
            "geometry":assert_native_geometry(session.port.clone(), &url).await}),
    );
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    fs::remove_dir(directory).unwrap();
}
