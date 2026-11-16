//! Native viewport input proves the complete focused frame chain before typing.
use super::*;
use crate::browser::semantic::surface::{HumanInput, Metadata};
trait Surface: Port {
    fn activate(&self) -> impl std::future::Future<Output = crate::browser::Result<()>> + Send;
    fn human_input(
        &self,
        input: HumanInput,
    ) -> impl std::future::Future<Output = crate::browser::Result<()>> + Send;
    async fn metadata(&self) -> crate::browser::Result<(Frame, Metadata)>;
    async fn focused_frame(&self, frame: &Frame) -> crate::browser::Result<bool>;
    async fn fixture(&self, text: &str) -> crate::browser::Result<Value>;
    async fn screenshot(&self) -> crate::browser::Result<Value>;
}
macro_rules! surface {
    ($port:ty) => {
        impl Surface for $port {
            async fn activate(&self) -> crate::browser::Result<()> {
                self.activate(deadline()).await
            }
            async fn human_input(&self, input: HumanInput) -> crate::browser::Result<()> {
                self.human_input(input, deadline()).await
            }
            async fn metadata(&self) -> crate::browser::Result<(Frame, Metadata)> {
                self.metadata(deadline()).await
            }
            async fn focused_frame(&self, frame: &Frame) -> crate::browser::Result<bool> {
                self.focused_frame(frame, deadline()).await
            }
            async fn fixture(&self, text: &str) -> crate::browser::Result<Value> {
                self.fixture(text, deadline()).await
            }
            async fn screenshot(&self) -> crate::browser::Result<Value> {
                self.screenshot(deadline()).await
            }
        }
    };
}
surface!(engine::semantic_cdp::Semantic);
surface!(engine::bidi::page::Semantic);
async fn click<P: Surface>(page: &Page<P>, x: f64, y: f64) {
    page.command(true, false, deadline(), move |p| async move {
        p.human_input(HumanInput::Click { x, y }).await
    })
    .await
    .unwrap();
}
async fn assert_focus<P: Surface>(port: P, label: &str) {
    let page = Page::new(port.clone());
    let permit = page.claim_human(deadline()).await.unwrap();
    page.command(true, false, deadline(), move |p| async move {
        p.activate().await
    })
    .await
    .unwrap();
    let until = deadline();
    let frames = loop {
        let frames = page.access(true).unwrap().frames(deadline()).await.unwrap();
        if frames.len() == 3 {
            break frames;
        }
        assert!(Instant::now() < until, "three focus documents did not load");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let until = deadline();
    loop {
        if port
            .fixture("surfaceEvents.filter(e=>e.ready).length")
            .await
            .unwrap()
            == 3
        {
            break;
        }
        assert!(
            Instant::now() < until,
            "three documents did not reach painted readiness"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    port.screenshot().await.unwrap();
    click(&page, 100., 80.).await;
    let (top, meta) = page.access(true).unwrap().metadata().await.unwrap();
    let evidence=port.fixture("({hasFocus:document.hasFocus(),active:document.activeElement.id,hit:document.elementFromPoint(100,80).id})").await.unwrap();
    let focus = meta.focus.unwrap_or_else(|| panic!("{evidence}"));
    assert!(
        page.access(true)
            .unwrap()
            .focused_frame(&top)
            .await
            .unwrap()
    );
    page.command(true, false, deadline(), move |p| async move {
        p.human_input(HumanInput::Text {
            frame: Box::new(top),
            focus,
            text: "synthetic 中文".into(),
        })
        .await
    })
    .await
    .unwrap();
    assert_eq!(
        port.fixture("document.querySelector('#secret').value")
            .await
            .unwrap(),
        "synthetic 中文"
    );
    port.screenshot().await.unwrap();
    click(&page, 100., 224.).await;
    let authorized = page.access(true).unwrap();
    let mut focused = Vec::new();
    let mut checks = Vec::new();
    for frame in &frames {
        let Output::Surface(meta) = authorized
            .program(frame, Program::Surface, deadline())
            .await
            .unwrap()
        else {
            panic!("surface");
        };
        let chain = authorized.focused_frame(frame).await.unwrap();
        checks.push(json!({"url":meta.url,"focus":meta.focus.as_ref().map(|f|(&f.tag,&f.input_type,&f.value)),"chain":chain}));
        if let Some(focus) = meta.focus
            && chain
        {
            focused.push((frame.clone(), focus));
        }
    }
    let hit=port.fixture("({active:document.activeElement.outerHTML,hit:document.elementFromPoint(100,224).outerHTML,scroll:scrollY,events:globalThis.surfaceEvents})").await.unwrap();
    record(
        &format!("{label}-human-focus-checks.json"),
        &json!({"checks":checks,"hit":hit}),
    );
    assert_eq!(
        focused.len(),
        1,
        "one focused native control: {checks:?} {hit}"
    );
    let (frame, focus) = focused.pop().unwrap();
    assert!(frame.parent.is_some());
    let target = frame.clone();
    page.command(true, false, deadline(), move |p| async move {
        p.human_input(HumanInput::Text {
            frame: Box::new(target),
            focus,
            text: "nested 中文".into(),
        })
        .await
    })
    .await
    .unwrap();
    let Output::Surface(meta) = authorized
        .program(&frame, Program::Surface, deadline())
        .await
        .unwrap()
    else {
        panic!("surface");
    };
    let focus = meta.focus.unwrap();
    assert_eq!(focus.value, "nested 中文");
    page.command(true, false, deadline(), move |p| async move {
        p.human_input(HumanInput::Key(crate::browser::semantic::Key::Backspace))
            .await
    })
    .await
    .unwrap();
    let Output::Surface(meta) = authorized
        .program(&frame, Program::Surface, deadline())
        .await
        .unwrap()
    else {
        panic!("surface");
    };
    let focus = meta.focus.unwrap();
    assert_eq!(focus.value, "nested 中");
    let target = frame.clone();
    page.command(true, false, deadline(), move |p| async move {
        p.human_input(HumanInput::Text {
            frame: Box::new(target),
            focus,
            text: "文".into(),
        })
        .await
    })
    .await
    .unwrap();
    let Output::Surface(meta) = authorized
        .program(&frame, Program::Surface, deadline())
        .await
        .unwrap()
    else {
        panic!("surface");
    };
    let focus = meta.focus.unwrap();
    click(&page, 100., 80.).await;
    assert!(
        !page
            .access(true)
            .unwrap()
            .focused_frame(&frame)
            .await
            .unwrap()
    );
    let target = frame.clone();
    let error = page
        .command(true, false, deadline(), move |p| async move {
            p.human_input(HumanInput::Text {
                frame: Box::new(target),
                focus,
                text: "must not appear".into(),
            })
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_stale_frame", "not_sent")
    );
    let Output::Surface(meta) = page
        .access(true)
        .unwrap()
        .program(&frame, Program::Surface, deadline())
        .await
        .unwrap()
    else {
        panic!("surface");
    };
    assert!(meta.focus.is_none_or(|f| f.value == "nested 中文"));
    assert_eq!(
        port.fixture("frames[0].frames[0].document.querySelector('#secret').value")
            .await
            .unwrap(),
        "nested 中文"
    );
    if label == "bidi" {
        port.fixture("document.querySelector('iframe').style.transform='rotate(10deg)'")
            .await
            .unwrap();
        port.screenshot().await.unwrap();
        let before = port
            .fixture("surfaceEvents.filter(e=>e.target).length")
            .await
            .unwrap();
        let error = page
            .command(true, false, deadline(), move |p| async move {
                p.human_input(HumanInput::Click { x: 200., y: 260. }).await
            })
            .await
            .unwrap_err();
        assert_eq!(
            (error.code, error.state),
            ("browser_geometry_unsupported", "not_sent")
        );
        assert_eq!(
            port.fixture("surfaceEvents.filter(e=>e.target).length")
                .await
                .unwrap(),
            before
        );
    }
    page.return_to_ai(&permit, true, deadline()).await.unwrap();
    record(
        &format!("{label}-human-focus.json"),
        &json!({"root":evidence,"documents":frames.len(),"cross_origin_focus_chain":true,"inactive_frame_text_refused":true}),
    );
}
async fn site() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let telemetry = "<script>globalThis.surfaceEvents=[];addEventListener('load',()=>requestAnimationFrame(()=>requestAnimationFrame(()=>top.postMessage({rdhSurface:true,ready:true,url:location.href},'*'))));addEventListener('message',e=>{if(e.data.rdhSurface)surfaceEvents.push(e.data)});addEventListener('pointerdown',e=>top.postMessage({rdhSurface:true,url:location.href,target:e.target.outerHTML,x:e.clientX,y:e.clientY,rect:document.querySelector('input')?.getBoundingClientRect().toJSON()},'*'));</script>";
    let control = "<input id=secret type=password style='position:absolute;left:40px;top:60px;width:240px;height:40px'>";
    let top = format!(
        "<!doctype html><meta charset=utf-8><style>body{{margin:0}}</style>{telemetry}{control}<iframe style='position:absolute;left:0;top:160px;border:3px solid black;width:600px;height:300px;transform:scale(.8);transform-origin:0 0' src='{}middle'></iframe>",
        url.replace("127.0.0.1", "localhost")
    );
    let middle = format!(
        "<!doctype html><style>body{{margin:0}}</style>{telemetry}<iframe style='border:0;width:600px;height:280px' src='{url}grandchild'></iframe>"
    );
    let grand = format!(
        "<!doctype html><meta charset=utf-8><style>body{{margin:0}}</style>{telemetry}{control}"
    );
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(move || {
                let body = top.clone();
                async move { axum::response::Html(body) }
            }),
        )
        .route(
            "/middle",
            axum::routing::get(move || {
                let body = middle.clone();
                async move { axum::response::Html(body) }
            }),
        )
        .route(
            "/grandchild",
            axum::routing::get(move || {
                let body = grand.clone();
                async move { axum::response::Html(body) }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Firefox helper and evidence directory"]
async fn bidi_native_human_focus() {
    let (url, server) = site().await;
    let root = root("bidi-human-surface");
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let browser = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut session = engine::bidi::page::Session::open(
        &helper,
        &browser,
        &root,
        policy::Policy::new(Default::default()),
    )
    .await
    .unwrap();
    session.navigate(&url, deadline()).await.unwrap();
    assert_focus(session.port.clone(), "bidi").await;
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Chrome and evidence directory"]
async fn cdp_native_human_focus() {
    let (url, server) = site().await;
    let root = root("cdp-human-surface");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut session = engine::semantic_cdp::Session::launch(
        &browser,
        &root,
        policy::Policy::new(Default::default()),
        deadline(),
    )
    .await
    .unwrap();
    session.navigate(&url, deadline()).await.unwrap();
    assert_focus(session.port.clone(), "cdp").await;
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
