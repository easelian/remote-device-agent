use super::{observation, *};

#[derive(Clone, Copy)]
pub(super) enum Change {
    Cover,
    Hide,
    Offscreen,
    Scale,
    ChildNavigate,
    Cap,
}
pub(super) trait Harness: observation::Harness {
    fn mount(&self, base: &str) -> impl Future<Output = Result<()>> + Send;
    fn frame_change(&self, change: Change) -> impl Future<Output = Result<()>> + Send;
    fn frame_clicks(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
}
fn mount(base: &str) -> String {
    let foreign = base.replace("127.0.0.1", "localhost");
    let html = format!(
        "<button id='root-control'>Root</button><div><iframe id='first-frame' style='width:650px;height:360px;border:4px solid;margin:16px' src='{foreign}child'></iframe></div><iframe id='sibling-frame' style='width:650px;height:160px;border:4px solid' src='{base}sibling'></iframe>"
    );
    format!(
        "(async()=>{{if(globalThis.frameListener)removeEventListener('message',frameListener);frameClicks=[];frameListener=e=>{{if(e.data?.rdh_fixture==='click')frameClicks.push(e.data.id);}};addEventListener('message',frameListener);document.body.innerHTML={};scrollTo(0,0);await Promise.all([...document.querySelectorAll('iframe')].map(e=>new Promise(resolve=>e.addEventListener('load',resolve,{{once:true}}))));return true;}})()",
        serde_json::to_string(&html).unwrap()
    )
}
fn change(change: Change) -> &'static str {
    match change {
        Change::Cover => {
            "(()=>{scrollTo(0,0);const r=document.getElementById('first-frame').getBoundingClientRect(),e=document.createElement('div');e.id='frame-cover';Object.assign(e.style,{position:'fixed',zIndex:'99999',background:'black',left:r.left+'px',top:r.top+'px',width:r.width+'px',height:r.height+'px'});document.body.append(e);return true;})()"
        }
        Change::Hide => "(document.getElementById('first-frame').style.display='none',true)",
        Change::Offscreen => {
            "(document.getElementById('first-frame').style.marginTop='1800px',true)"
        }
        Change::Scale => {
            "(document.getElementById('first-frame').style.transform='scale(0.8)',document.getElementById('first-frame').style.transformOrigin='0 0',true)"
        }
        Change::ChildNavigate => {
            "(async()=>{const e=document.getElementById('first-frame');const ready=new Promise(resolve=>e.addEventListener('load',resolve,{once:true}));e.src=e.src+'?new-document=1';await ready;return true;})()"
        }
        Change::Cap => {
            "(async()=>{document.body.innerHTML='';const pending=[];for(let i=0;i<33;i++){const e=document.createElement('iframe');pending.push(new Promise(resolve=>e.addEventListener('load',resolve,{once:true})));e.srcdoc='<button>Frame '+i+'</button>';document.body.append(e);}await Promise.all(pending);return true;})()"
        }
    }
}
macro_rules! native_harness {
    ($ty:ty) => {
        impl Harness for $ty {
            async fn mount(&self, base: &str) -> Result<()> {
                self.fixture(&mount(base), deadline()).await?;
                let until = deadline();
                loop {
                    if let Ok(frames) = self.frames(until).await {
                        let mut ready = frames.len() == 4
                            && frames
                                .iter()
                                .any(|frame| frame.url.ends_with("/grandchild"));
                        for frame in &frames {
                            ready &= matches!(
                                self.program(frame, Program::Ready, until).await,
                                Ok(Output::Applied(Some(true)))
                            );
                        }
                        if ready {
                            return Ok(());
                        }
                    }
                    if Instant::now() >= until {
                        return Err(crate::browser::Failure::new("fixture_not_ready"));
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            async fn frame_change(&self, value: Change) -> Result<()> {
                self.fixture(change(value), deadline()).await?;
                if matches!(value, Change::ChildNavigate) {
                    let until = deadline();
                    loop {
                        if let Ok(frames) = self.frames(until).await {
                            let mut ready = frames.len() == 4
                                && frames
                                    .iter()
                                    .any(|frame| frame.url.ends_with("/child?new-document=1"));
                            for frame in &frames {
                                ready &= matches!(
                                    self.program(frame, Program::Ready, until).await,
                                    Ok(Output::Applied(Some(true)))
                                );
                            }
                            if ready {
                                return Ok(());
                            }
                        }
                        if Instant::now() >= until {
                            return Err(crate::browser::Failure::new("fixture_not_ready"));
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
                Ok(())
            }
            async fn frame_clicks(&self) -> Result<Vec<String>> {
                serde_json::from_value(self.fixture("frameClicks", deadline()).await?)
                    .map_err(|_| types::decode())
            }
        }
    };
}
native_harness!(engine::semantic_cdp::Semantic);
native_harness!(engine::bidi::page::Semantic);
impl Harness for Fake {
    async fn mount(&self, base: &str) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        let template = data.rows[0].clone();
        let mut ids = Identities::new();
        let mut frame = |key: &str, parent, path: &str| {
            ids.observe(
                key,
                parent,
                "doc",
                "realm",
                Marker {
                    realm: key.into(),
                    restoration: 0,
                    url: format!("{base}{path}"),
                },
            )
            .unwrap()
        };
        let root = frame("root", None, "");
        let child = frame("child", Some(root.context), "child");
        let deep = frame("deep", Some(child.context), "grandchild");
        let sibling = frame("sibling", Some(root.context), "sibling");
        data.rows = vec![observation::fact(
            &template,
            "root-control",
            3000,
            "button",
            "Root",
        )];
        data.frame = root;
        data.children = vec![
            (
                child,
                vec![observation::fact(
                    &template, "child", 3001, "button", "Child",
                )],
            ),
            (
                deep,
                vec![observation::fact(&template, "deep", 3002, "button", "Deep")],
            ),
            (
                sibling,
                vec![observation::fact(
                    &template, "sibling", 3003, "button", "Sibling",
                )],
            ),
        ];
        data.events.clear();
        Ok(())
    }
    async fn frame_change(&self, change: Change) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        match change {
            Change::Cover => data.children[1].1[0].hit = false,
            Change::Hide => data.children[1].1[0].visible = false,
            Change::Offscreen => {
                data.children[1].1[0].in_view = false;
                data.children[1].1[0].hit = false;
            }
            Change::Scale => {}
            Change::ChildNavigate => {
                data.children[0].0.document += 1;
                data.children[1].0.document += 1;
            }
            Change::Cap => {
                let template = data.children[0].clone();
                data.children = (0..33).map(|_| template.clone()).collect();
            }
        }
        Ok(())
    }
    async fn frame_clicks(&self) -> Result<Vec<String>> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| event["type"] == "click")
            .map(|event| event["id"].as_str().unwrap().to_owned())
            .collect())
    }
}
async fn expect_clicks<P: Harness>(port: &P, expected: &[&str]) {
    let until = deadline();
    loop {
        let actual = port.frame_clicks().await.unwrap();
        if actual.len() >= expected.len() {
            assert_eq!(actual, expected);
            break;
        }
        assert!(Instant::now() < until, "frame click not observed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
pub(super) async fn assert_frames<P: Harness>(port: P, base: &str) -> Value {
    let page = Page::new(port.clone());
    port.mount(base).await.unwrap();
    let frames = port.frames(deadline()).await.unwrap();
    if frames.len() != 4 {
        let diagnostic = page
            .observe(Query::default(), 200, 64000, None, deadline())
            .await;
        eprintln!(
            "frame diagnostic: {}",
            diagnostic
                .map(|read| serde_json::to_value(read).unwrap())
                .unwrap_or_else(|error| json!({"error":format!("{error:?}")}))
        );
    }
    assert_eq!(
        frames.len(),
        4,
        "frame snapshot: {}",
        serde_json::to_string(&frames).unwrap()
    );
    let observed = observation::read(&page).await;
    let deep = observed
        .rows
        .iter()
        .find(|row| row.facts.id == "deep")
        .unwrap();
    let deep_frame = frames
        .iter()
        .find(|frame| frame.context == deep.context)
        .unwrap();
    let parent = frames
        .iter()
        .find(|frame| Some(frame.context) == deep_frame.parent)
        .unwrap();
    assert!(parent.parent.is_some());
    let within = page
        .observe(
            Query {
                within: Some(deep.context),
                ..Default::default()
            },
            200,
            64000,
            None,
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(
        within
            .rows
            .iter()
            .filter(|row| row.reference.is_some())
            .map(|row| row.facts.id.as_str())
            .collect::<Vec<_>>(),
        vec!["deep"]
    );
    page.act(
        observation::reference(&within, "deep"),
        Action::Click,
        deadline(),
    )
    .await
    .unwrap();
    expect_clicks(&port, &["deep"]).await;
    for modification in [Change::Offscreen, Change::Scale] {
        port.mount(base).await.unwrap();
        let observed = observation::read(&page).await;
        let deep = observation::reference(&observed, "deep");
        port.frame_change(modification).await.unwrap();
        page.act(deep, Action::Click, deadline()).await.unwrap();
        expect_clicks(&port, &["deep"]).await;
    }
    for modification in [Change::Cover, Change::Hide] {
        port.mount(base).await.unwrap();
        let deep = observation::reference(&observation::read(&page).await, "deep");
        port.frame_change(modification).await.unwrap();
        let error = page.act(deep, Action::Click, deadline()).await.unwrap_err();
        assert_eq!(error.state, "not_sent");
        assert!(port.frame_clicks().await.unwrap().is_empty());
    }
    port.mount(base).await.unwrap();
    let observed = observation::read(&page).await;
    let deep = observation::reference(&observed, "deep");
    let sibling = observation::reference(&observed, "sibling");
    port.frame_change(Change::ChildNavigate).await.unwrap();
    let error = page.act(deep, Action::Click, deadline()).await.unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_stale_reference", "not_sent")
    );
    page.act(sibling, Action::Click, deadline()).await.unwrap();
    expect_clicks(&port, &["sibling"]).await;
    port.frame_change(Change::Cap).await.unwrap();
    let error = page
        .observe(Query::default(), 200, 64000, None, deadline())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "browser_frame_limit");
    port.mount(base).await.unwrap();
    assert_eq!(port.frames(deadline()).await.unwrap().len(), 4);
    json!({"nested_cross_origin_frames":4,"within_context":true,"actual_click_target":true,"scrolled_ancestors":true,"scaled_frame":true,"covered_hidden_ancestor_rejected":true,"child_navigation_preserves_sibling":true,"frame_cap":32,"after_churn_enumeration":true})
}
#[tokio::test]
async fn fake_frames_use_shared_scope_ancestry_navigation_and_geometry_assertions() {
    assert_frames(Fake::new(), "http://fixture/").await;
}
