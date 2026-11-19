use super::*;

#[derive(Clone, Copy)]
pub(super) enum Scene {
    Records,
    Limits,
    Shadow,
    Controls,
}
#[derive(Clone, Copy)]
pub(super) enum Change {
    Other,
    Own,
    Replace,
    ReplaceAtDispatch,
    Type,
    Hash,
    History,
    Occlude,
    Reveal,
    Detach,
}
pub(super) trait Harness: input::Harness {
    fn scene(&self, scene: Scene) -> impl Future<Output = Result<()>> + Send;
    fn change(&self, change: Change) -> impl Future<Output = Result<()>> + Send;
}
fn scene_html(scene: Scene) -> &'static str {
    match scene {
        Scene::Records => {
            "<section data-rdh-record='a'><p id='own'>Invoice-A 10</p><button id='a'>Approve A</button></section><section data-rdh-record='b'><p id='other'>Invoice-B 20</p><button id='b'>Approve B</button></section><section data-rdh-record='c' style='margin-top:2000px'><button id='c'>Below viewport</button></section>"
        }
        Scene::Limits => {
            "<section data-rdh-record='large'><p id='huge'></p><button id='large'>Large record</button></section><button id='long-name'></button>"
        }
        Scene::Shadow => {
            "<span id='outer-label'>Outer label</span><div id='host'></div><div id='closed-host'></div><canvas id='canvas' width=40 height=20></canvas>"
        }
        Scene::Controls => {
            "<label>Check<input id='check' type='checkbox'></label><select id='select' aria-label='Choose'><option value='a'>A</option><option value='b'>B</option><option value='disabled' disabled>Disabled</option></select><input id='date' type='date' aria-label='Date'><input id='color' type='color' aria-label='Color'><input id='range' type='range' min='0' max='100' step='1' aria-label='Range'><input id='secret' type='password' aria-label='Secret' value='never-observe-this'><input id='readonly' readonly aria-label='Read only' value='fixed'><div id='inferred' tabindex='0'>Inferred</div><textarea id='keys' aria-label='Keys'>x</textarea><div id='outer' role='button' style='display:inline-block;padding:0'><button id='inner' style='margin:0'>Nested</button></div><button id='disabled' disabled>Disabled</button>"
        }
    }
}
fn scene_expression(scene: Scene) -> String {
    let extra = match scene {
        Scene::Limits => {
            "document.getElementById('huge').textContent='x'.repeat(5000);document.getElementById('long-name').textContent='名'.repeat(600);"
        }
        Scene::Shadow => {
            "document.getElementById('host').attachShadow({mode:'open'}).innerHTML=\"<button id='shadow-good'>Shadow text</button><button id='shadow-bad' aria-labelledby='outer-label'>Fallback</button>\";document.getElementById('closed-host').attachShadow({mode:'closed'}).innerHTML=\"<button id='closed'>Closed</button>\";"
        }
        _ => "",
    };
    format!(
        "(()=>{{document.body.innerHTML={};events=[];scrollTo(0,0);{extra}return true;}})()",
        serde_json::to_string(scene_html(scene)).unwrap()
    )
}
fn change_expression(change: Change) -> &'static str {
    match change {
        Change::Other => "(document.getElementById('other').textContent='Invoice-B changed',true)",
        Change::Own => "(document.getElementById('own').textContent='Invoice-A changed',true)",
        Change::Replace => {
            "(()=>{const a=document.getElementById('a');a.replaceWith(a.cloneNode(true));return true;})()"
        }
        Change::ReplaceAtDispatch => {
            "(()=>{const a=document.getElementById('a'),replacement=a.cloneNode(true);replacement.id='replacement';a.replaceWith(replacement);return true;})()"
        }
        Change::Type => "(document.getElementById('a').setAttribute('role','link'),true)",
        Change::Hash => "(location.hash='same-document',true)",
        Change::History => "(history.pushState({},'',location.pathname+'?same-document=1'),true)",
        Change::Occlude => {
            "(()=>{const e=document.getElementById('a'),r=e.getBoundingClientRect(),cover=document.createElement('div');cover.id='cover';Object.assign(cover.style,{position:'fixed',zIndex:'999999',left:r.left+'px',top:r.top+'px',width:r.width+'px',height:r.height+'px',background:'black'});document.body.append(cover);return true;})()"
        }
        Change::Reveal => "(document.getElementById('cover')?.remove(),true)",
        Change::Detach => "(document.getElementById('a').remove(),true)",
    }
}
macro_rules! native_harness {
    ($ty:ty) => {
        impl Harness for $ty {
            async fn scene(&self, scene: Scene) -> Result<()> {
                self.fixture(&scene_expression(scene), deadline())
                    .await
                    .map(drop)
            }
            async fn change(&self, change: Change) -> Result<()> {
                self.fixture(change_expression(change), deadline())
                    .await
                    .map(drop)
            }
        }
    };
}
native_harness!(engine::semantic_cdp::Semantic);
native_harness!(engine::bidi::page::Semantic);

pub(super) fn fact(template: &Facts, id: &str, node: u64, role: &str, name: &str) -> Facts {
    let mut row = template.clone();
    row.id = id.into();
    row.node = node;
    row.name = name.into();
    row.role = role.into();
    row.kind = "native".into();
    row.tag = "button".into();
    row.input_type = String::new();
    row.name_source = "content".into();
    row.name_quality.clear();
    row.value = None;
    row.editable = false;
    row.checked = false;
    row.enabled = true;
    row.readonly = false;
    row.sensitive = false;
    row.visible = true;
    row.hit = true;
    row.in_view = true;
    row.record = Material {
        owner_list: None,
        owner_context: Vec::new(),
        entries: vec![Entry {
            identity: node,
            tag: "button".into(),
            attributes: vec![],
            text: name.into(),
            value: None,
            checked: false,
        }],
        complete: true,
        limits: vec![],
    };
    row
}
impl Harness for Fake {
    async fn scene(&self, scene: Scene) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        let template = data.rows[0].clone();
        data.events.clear();
        data.children.clear();
        data.position = 0;
        data.sequence = 0;
        data.filtered.clear();
        data.rows = match scene {
            Scene::Records => {
                let mut a = fact(&template, "a", 1001, "button", "Approve A");
                a.record.entries[0].text = "Invoice-A 10".into();
                let mut b = fact(&template, "b", 1002, "button", "Approve B");
                b.record.entries[0].text = "Invoice-B 20".into();
                let mut c = fact(&template, "c", 1003, "button", "Below viewport");
                c.in_view = false;
                c.hit = false;
                vec![a, b, c]
            }
            Scene::Limits => {
                let mut a = fact(&template, "large", 1010, "button", "Large record");
                a.record.complete = false;
                a.record.limits.push("record_character_limit".into());
                let mut b = fact(&template, "long-name", 1011, "button", &"名".repeat(512));
                b.name_quality.push("name_character_limit".into());
                vec![a, b]
            }
            Scene::Shadow => {
                let a = fact(&template, "shadow-good", 1020, "button", "Shadow text");
                let mut b = fact(&template, "shadow-bad", 1021, "button", "Fallback");
                b.name_quality.push("missing_reference".into());
                vec![a, b]
            }
            Scene::Controls => {
                let specs = [
                    ("check", "checkbox", "input", "checkbox", None),
                    ("select", "combobox", "select", "select-one", Some("a")),
                    ("date", "textbox", "input", "date", Some("")),
                    ("color", "textbox", "input", "color", Some("#000000")),
                    ("range", "slider", "input", "range", Some("50")),
                    ("secret", "textbox", "input", "password", None),
                    ("readonly", "textbox", "input", "text", Some("fixed")),
                    ("inferred", "inferred", "div", "", None),
                    ("keys", "textbox", "textarea", "textarea", Some("x")),
                    ("outer", "button", "div", "", None),
                    ("inner", "button", "button", "submit", None),
                    ("disabled", "button", "button", "submit", None),
                ];
                specs
                    .into_iter()
                    .enumerate()
                    .map(|(index, (id, role, tag, kind, value))| {
                        let mut row = fact(&template, id, 2000 + index as u64, role, id);
                        row.tag = tag.into();
                        row.input_type = kind.into();
                        row.value = value.map(str::to_owned);
                        row.readonly = id == "readonly";
                        row.sensitive = id == "secret";
                        row.editable = matches!(role, "textbox");
                        row.enabled = id != "disabled";
                        row.hit = id != "outer";
                        if id == "inferred" {
                            row.kind = "inferred".into();
                        }
                        row
                    })
                    .collect()
            }
        };
        Ok(())
    }
    async fn change(&self, change: Change) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        match change {
            Change::Other => {
                data.rows
                    .iter_mut()
                    .find(|row| row.id == "b")
                    .unwrap()
                    .record
                    .entries[0]
                    .text = "Invoice-B changed".into()
            }
            Change::Own => {
                data.rows
                    .iter_mut()
                    .find(|row| row.id == "a")
                    .unwrap()
                    .record
                    .entries[0]
                    .text = "Invoice-A changed".into()
            }
            Change::Replace => {
                data.rows.iter_mut().find(|row| row.id == "a").unwrap().node += 10000
            }
            Change::ReplaceAtDispatch => {
                let row = data.rows.iter_mut().find(|row| row.id == "a").unwrap();
                row.node += 10000;
                row.id = "replacement".into();
            }
            Change::Type => {
                data.rows.iter_mut().find(|row| row.id == "a").unwrap().role = "link".into()
            }
            Change::Hash => data.frame.url = "http://fixture/#same-document".into(),
            Change::History => data.frame.url = "http://fixture/?same-document=1".into(),
            Change::Occlude => data.rows.iter_mut().find(|row| row.id == "a").unwrap().hit = false,
            Change::Reveal => data.rows.iter_mut().find(|row| row.id == "a").unwrap().hit = true,
            Change::Detach => data.rows.retain(|row| row.id != "a"),
        }
        Ok(())
    }
}
pub(super) async fn read<P: Port>(page: &Page<P>) -> Observation {
    page.observe(
        Query {
            filter: Filter::Interactive,
            ..Default::default()
        },
        200,
        64000,
        None,
        deadline(),
    )
    .await
    .unwrap()
}
pub(super) fn reference(read: &Observation, id: &str) -> Reference {
    read.rows
        .iter()
        .find(|row| row.facts.id == id)
        .unwrap_or_else(|| panic!("missing {id}"))
        .reference
        .clone()
        .unwrap_or_else(|| panic!("missing reference {id}"))
}
async fn clicks<P: input::Harness>(port: &P, id: &str) -> usize {
    port.evidence(id).await.unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "click")
        .count()
}
pub(super) async fn assert_observation<P: Harness>(port: P) -> Value {
    let page = Page::new(port.clone());
    port.scene(Scene::Records).await.unwrap();
    let initial = read(&page).await;
    let a = reference(&initial, "a");
    port.change(Change::Other).await.unwrap();
    page.act(a.clone(), Action::Click, deadline())
        .await
        .unwrap();
    assert_eq!(clicks(&port, "a").await, 1);
    port.change(Change::Own).await.unwrap();
    let error = page.act(a, Action::Click, deadline()).await.unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_stale_reference", "not_sent")
    );
    assert_eq!(clicks(&port, "a").await, 1);
    for change in [Change::Replace, Change::Type, Change::Detach] {
        port.scene(Scene::Records).await.unwrap();
        let a = reference(&read(&page).await, "a");
        port.change(change).await.unwrap();
        let error = page.act(a, Action::Click, deadline()).await.unwrap_err();
        assert_eq!(
            (error.code, error.state),
            ("browser_stale_reference", "not_sent")
        );
    }
    port.scene(Scene::Records).await.unwrap();
    let a = reference(&read(&page).await, "a");
    for change in [Change::Hash, Change::History] {
        port.change(change).await.unwrap();
        page.act(a.clone(), Action::Click, deadline())
            .await
            .unwrap();
    }
    assert_eq!(clicks(&port, "a").await, 2);
    port.change(Change::Occlude).await.unwrap();
    let error = page
        .act(a.clone(), Action::Click, deadline())
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_not_actionable", "not_sent")
    );
    port.change(Change::Reveal).await.unwrap();
    page.act(a.clone(), Action::Click, deadline())
        .await
        .unwrap();
    let fresh = read(&page).await;
    assert!(page.act(a, Action::Click, deadline()).await.is_err());
    let below = reference(&fresh, "c");
    page.act(below, Action::Click, deadline()).await.unwrap();
    assert_eq!(clicks(&port, "c").await, 4);
    port.scene(Scene::Records).await.unwrap();
    let query = Query {
        filter: Filter::Interactive,
        ..Default::default()
    };
    let mut token = None;
    let mut ids = Vec::new();
    let mut old = None;
    for _ in 0..10 {
        let chunk = page
            .observe(query.clone(), 1, 64000, token, deadline())
            .await
            .unwrap();
        assert!(chunk.rows.len() <= 1);
        if let Some(previous) = old.take() {
            assert!(page.act(previous, Action::Click, deadline()).await.is_err());
        }
        ids.extend(chunk.rows.iter().map(|row| row.facts.id.clone()));
        old = chunk.rows.first().and_then(|row| row.reference.clone());
        if chunk.cursor.is_none() {
            assert!(chunk.complete);
            break;
        }
        assert!(!chunk.complete);
        token = chunk.cursor;
    }
    assert_eq!(ids, vec!["a", "b", "c"]);
    let found = page
        .observe(
            Query {
                filter: Filter::Interactive,
                find: Some("Invoice-B".into()),
                within: None,
            },
            200,
            64000,
            None,
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(
        found
            .rows
            .iter()
            .map(|row| row.facts.id.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
    let viewport = page
        .observe(
            Query {
                filter: Filter::Viewport,
                ..Default::default()
            },
            200,
            64000,
            None,
            deadline(),
        )
        .await
        .unwrap();
    assert!(!viewport.rows.iter().any(|row| row.facts.id == "c"));
    port.scene(Scene::Limits).await.unwrap();
    let limits = read(&page).await;
    for id in ["large", "long-name"] {
        assert!(
            limits
                .rows
                .iter()
                .find(|row| row.facts.id == id)
                .unwrap()
                .reference
                .is_none()
        );
    }
    port.scene(Scene::Shadow).await.unwrap();
    let shadow = read(&page).await;
    let good = shadow
        .rows
        .iter()
        .find(|row| row.facts.id == "shadow-good")
        .unwrap();
    assert_eq!(good.facts.name, "Shadow text");
    assert!(good.reference.is_some());
    let bad = shadow
        .rows
        .iter()
        .find(|row| row.facts.id == "shadow-bad")
        .unwrap();
    assert_eq!(bad.facts.name_quality, vec!["missing_reference"]);
    assert!(bad.reference.is_none());
    assert!(!shadow.rows.iter().any(|row| row.facts.id == "closed"));
    assert!(
        shadow
            .omitted
            .contains(&"closed_shadow_not_observable".into())
    );
    assert!(
        shadow
            .omitted
            .contains(&"canvas_pixels_outside_profile".into())
    );
    json!({"unrelated_record_preserved":true,"own_record_rejected":true,"replaced_detached_retyped_rejected":true,"hash_history_preserved":true,"occlusion_rejected":true,"scroll_verified":true,"pagination":ids,"find_record_text":true,"viewport_filter":true,"incomplete_and_long_name_no_ref":true,"shadow_tree_scope":true})
}

#[tokio::test]
async fn fake_observation_uses_shared_records_queries_and_reference_assertions() {
    assert_observation(Fake::new()).await;
}
#[tokio::test]
async fn references_cursors_and_human_grants_cannot_cross_sessions() {
    let first = Page::new(Fake::new());
    let second = Page::new(Fake::new());
    let query = Query::default();
    let a = first
        .observe(query.clone(), 1, 64000, None, deadline())
        .await
        .unwrap();
    let b = second
        .observe(query.clone(), 1, 64000, None, deadline())
        .await
        .unwrap();
    assert_ne!(a.rows[0].reference, b.rows[0].reference);
    assert!(
        second
            .act(
                a.rows[0].reference.clone().unwrap(),
                Action::Click,
                deadline()
            )
            .await
            .is_err()
    );
    assert!(
        second
            .observe(query, 1, 64000, a.cursor, deadline())
            .await
            .is_err()
    );
    let permit = first.claim_human(deadline()).await.unwrap();
    assert!(
        second
            .human_observe(&permit, Query::default(), 200, 64000, deadline())
            .await
            .is_err()
    );
}
