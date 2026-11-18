use super::*;
use crate::browser::engine;
use futures_util::FutureExt;
use std::path::PathBuf;
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(15)
}
struct Fixture {
    native: native::Session,
    port: native::Port,
    page: sem::Page<native::Port>,
    registry: Registry,
    directory: PathBuf,
    backend: &'static str,
}
impl Fixture {
    async fn open(backend: &'static str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("rdh-dom-evidence-{backend}-{}", token().unwrap()));
        crate::storage::create_private_dir(&directory).unwrap();
        let policy = sem::policy::Policy::new(Default::default());
        let native = if backend == "cdp" {
            native::Session::Cdp(
                engine::semantic_cdp::Session::launch(
                    &PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap()),
                    &directory,
                    policy,
                    deadline(),
                )
                .await
                .unwrap(),
            )
        } else {
            native::Session::Bidi(
                engine::bidi::page::Session::open(
                    &PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap()),
                    &PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap()),
                    &directory,
                    policy,
                )
                .await
                .unwrap(),
            )
        };
        let port = native.port();
        Self {
            page: sem::Page::new(port.clone()),
            port,
            native,
            registry: Registry::default(),
            directory,
            backend,
        }
    }
    async fn fixture(&self, script: &str) -> Value {
        match &self.port {
            native::Port::Cdp(p) => p.fixture(script, deadline()).await.unwrap(),
            native::Port::Bidi(p) => p.fixture(script, deadline()).await.unwrap(),
        }
    }
    async fn read(&mut self, query: Value) -> Result<Value> {
        let query = Query::parse(&query)?;
        let tab = self.port.tab_id()?;
        let epoch = self.page.control().snapshot().0;
        let target = query
            .id()
            .map(|id| self.registry.lookup(id, &tab, epoch))
            .transpose()?;
        let capture = self.page.evidence(&query, target, deadline()).await?;
        let (mut value, mut prepared) = prepare(capture, &query, &tab, epoch)?;
        value["session_id"] = "fixture".into();
        value["control_epoch"] = epoch.into();
        value["selection"] =
            json!({"version":2,"backend":self.backend,"observation_profile":"semantic_dom_v1"});
        bounded(&mut value, &mut prepared)?;
        self.registry.publish(prepared);
        Ok(value)
    }
    async fn one(&mut self, text: &str) -> String {
        let found = self
            .read(json!({"kind":"find_nodes","tag":"div","text":text}))
            .await
            .unwrap();
        assert_eq!(found["nodes"].as_array().unwrap().len(), 1, "{found}");
        found["nodes"][0]["node_id"].as_str().unwrap().into()
    }
}
async fn check(backend: &'static str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async {
                axum::response::Html(include_str!(
                    "../../../../tests/fixtures/dom-evidence-v1/page.html"
                ))
            }),
        )
        .route(
            "/child",
            axum::routing::get(|| async {
                axum::response::Html(include_str!(
                    "../../../../tests/fixtures/dom-evidence-v1/frame.html"
                ))
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut f = Fixture::open(backend).await;
    let result=std::panic::AssertUnwindSafe(async {
        f.port.navigate(&origin,deadline()).await.unwrap();
        for _ in 0..100 {if f.fixture("window.evidenceReady===true").await==true {break;}tokio::time::sleep(Duration::from_millis(20)).await;}
        assert_eq!(f.fixture("window.evidenceReady===true").await,true);
        let ordinary=f.page.observe(sem::Query::default(),1000,64000,None,deadline()).await.unwrap();
        let action=ordinary.rows.iter().find(|r|r.facts.name=="Witness action").unwrap().reference.clone().unwrap();
        let witness=f.fixture("({events:window.evidenceWitness,x:scrollX,y:scrollY,focus:document.activeElement.id,requests:performance.getEntriesByType('resource').length})").await;
        let oracle:Value=serde_json::from_str(include_str!("../../../../tests/fixtures/dom-evidence-v1/oracle.json")).unwrap();
        for item in oracle["find"].as_array().unwrap() {
            let found=f.read(item["query"].clone()).await.unwrap();let mut projected=found["nodes"].clone();
            for node in projected.as_array_mut().unwrap() {for key in ["node_id","frame_index","main_frame"] {node.as_object_mut().unwrap().remove(key);}}
            assert_eq!(projected,item["nodes"],"{backend}: {found}");
        }
        let parent=f.one("Public prefix").await;let detail=f.read(json!({"kind":"read_node","node_id":parent})).await.unwrap();
        assert!(detail["node"]["text_excerpt"].as_str().unwrap().contains("Public suffix"));
        for word in oracle["forbidden"].as_array().unwrap() {assert!(!detail.to_string().contains(word.as_str().unwrap()));}
        let hidden=f.one("Hidden facts").await;let detail=f.read(json!({"kind":"read_node","node_id":hidden})).await.unwrap();assert_eq!(detail["node"]["attributes"]["hidden"],"");
        let covered=f.one("Covered region").await;let layout=f.read(json!({"kind":"read_layout","node_id":covered})).await.unwrap();
        assert_eq!(layout["layout"]["coordinate_space"],"frame_viewport");assert_eq!(layout["layout"]["hit"]["kind"],"other","{layout}");
        let blocker=f.read(json!({"kind":"read_node","node_id":layout["layout"]["hit"]["node_id"]})).await.unwrap();assert_eq!(blocker["node"]["name"],"Overlay");
        let outside=f.one("Outside region").await;let layout=f.read(json!({"kind":"read_layout","node_id":outside})).await.unwrap();assert_eq!(layout["layout"]["hit"]["kind"],"outside_viewport");
        let current=f.fixture("({events:window.evidenceWitness,x:scrollX,y:scrollY,focus:document.activeElement.id,requests:performance.getEntriesByType('resource').length})").await;assert_eq!(witness,current);
        f.page.diagnose(action,sem::actionability::ActionKind::Click,deadline()).await.unwrap();
        f.page.observe(sem::Query::default(),1000,64000,None,deadline()).await.unwrap();assert!(f.read(json!({"kind":"read_node","node_id":covered})).await.is_ok());
        f.fixture("(()=>{document.querySelector('#covered').replaceWith(document.querySelector('#covered').cloneNode(true));return true;})()").await;
        assert_eq!(f.read(json!({"kind":"read_node","node_id":covered})).await.unwrap_err().code,"browser_stale_reference");
        f.fixture(&format!("(()=>{{window.evidenceFramesLoaded=0;for(const host of ['127.0.0.1','localhost']){{const e=document.createElement('iframe');e.onload=()=>window.evidenceFramesLoaded++;e.src='http://'+host+':{}/child';document.body.append(e);}}return true;}})()",address.port())).await;
        for _ in 0..100 {if f.fixture("window.evidenceFramesLoaded===2").await==true {break;}tokio::time::sleep(Duration::from_millis(20)).await;}
        assert_eq!(f.fixture("window.evidenceFramesLoaded===2").await,true);
        let all=f.read(json!({"kind":"find_nodes","role":"status","scope":"all_frames"})).await.unwrap();
        assert_eq!(all["nodes"].as_array().unwrap().iter().filter(|v|v["name"]=="Frame state").count(),2,"{all}");
        let old=f.one("Hidden facts").await;
        f.port.navigate(&origin,deadline()).await.unwrap();
        assert_eq!(f.read(json!({"kind":"read_node","node_id":old})).await.unwrap_err().code,"browser_stale_reference");
        println!("{backend}: DOM evidence oracle, sensitive text, local hit, identity replacement/reload, frames and preserved action refs passed");
    }).catch_unwind().await;
    let closed = f.native.close().await;
    server.abort();
    assert!(closed.is_ok(), "cleanup: {closed:?}");
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_dom_evidence_cdp() {
    check("cdp").await;
}
#[tokio::test]
#[ignore = "requires native Firefox and namespace helper"]
async fn native_dom_evidence_bidi() {
    check("bidi").await;
}

async fn bounded_scene(backend: &'static str) {
    let mut f = Fixture::open(backend).await;
    let result=std::panic::AssertUnwindSafe(async {
        for (script,limit,counter,expected) in [
            ("(()=>{document.body.replaceChildren();const e=document.body.appendChild(document.createElement('div'));for(let i=0;i<6000;i++)e.appendChild(document.createComment('x'));return true;})()", "other_node_limit", "visited_other_nodes", 5000),
            ("(()=>{document.body.replaceChildren();for(let i=0;i<6000;i++)document.body.appendChild(document.createElement('span'));return true;})()", "element_limit", "visited_elements", 5000),
            ("(()=>{document.body.replaceChildren();for(let i=0;i<11000;i++)document.body.appendChild(document.createTextNode('x'));return true;})()", "text_node_limit", "visited_text_nodes", 10000),
        ] {
            f.fixture(script).await;
            let result=f.read(json!({"kind":"find_nodes","tag":"button"})).await.unwrap();
            assert_eq!(result["coverage"][counter],expected,"{result}");
            assert!(result["coverage"]["limits"].as_array().unwrap().iter().any(|v|v==limit),"{result}");
            assert_eq!(result["coverage"]["truncated"],true);
        }
        f.fixture("(()=>{document.body.replaceChildren();let e=document.body;for(let i=0;i<100;i++)e=e.appendChild(document.createElement('div'));e.appendChild(document.createElement('button'));return true;})()").await;
        let deep=f.read(json!({"kind":"find_nodes","tag":"button"})).await.unwrap();assert!(deep["nodes"].as_array().unwrap().is_empty());assert!(deep["coverage"]["limits"].as_array().unwrap().iter().any(|v|v=="depth_limit"));
        f.fixture("(()=>{document.body.replaceChildren();const e=document.body.appendChild(document.createElement('div'));e.textContent='😀'.repeat(1100);return true;})()").await;
        let found=f.read(json!({"kind":"find_nodes","tag":"div"})).await.unwrap();let id=found["nodes"][0]["node_id"].clone();assert_eq!(found["nodes"][0]["name"].as_str().unwrap().chars().count(),256);
        let detail=f.read(json!({"kind":"read_node","node_id":id})).await.unwrap();assert_eq!(detail["node"]["text_excerpt"].as_str().unwrap().chars().count(),1024);assert!(detail["coverage"]["limits"].as_array().unwrap().iter().any(|v|v=="excerpt_limit"));
        f.fixture("(()=>{document.body.replaceChildren();for(let i=0;i<64;i++){const e=document.body.appendChild(document.createElement('p'));e.textContent='😀'.repeat(256);}return true;})()").await;
        let large=f.read(json!({"kind":"find_nodes","tag":"p","limit":64})).await.unwrap();assert!(serde_json::to_vec(&large).unwrap().len()<=dom::MAX_BYTES);assert!(large["nodes"].as_array().unwrap().len()<64);assert!(large["coverage"]["limits"].as_array().unwrap().iter().any(|v|v=="byte_limit"));
        f.fixture("(()=>{document.body.replaceChildren();const e=document.body.appendChild(document.createElement('div'));e.setAttribute('aria-labelledby','   ');e.textContent='Fallback name';return true;})()").await;
        let blank=f.read(json!({"kind":"find_nodes","tag":"div","text":"Fallback name"})).await.unwrap();
        assert_eq!(blank["nodes"][0]["name"],"Fallback name");assert_eq!(blank["nodes"][0]["name_source"],"direct_text");
        f.fixture("(()=>{document.body.replaceChildren();const root=document.body.appendChild(document.createElement('div'));root.style.cssText='position:fixed;left:0;top:0;width:100px;height:100px';root.appendChild(document.createTextNode('Deep ancestor'));let e=root;for(let i=0;i<100;i++){e=e.appendChild(document.createElement('div'));e.style.cssText='width:100%;height:100%';}e.appendChild(document.createTextNode('Deep hit'));return true;})()").await;
        let deep=f.read(json!({"kind":"find_nodes","tag":"div","text":"Deep ancestor","limit":1})).await.unwrap();
        let layout=f.read(json!({"kind":"read_layout","node_id":deep["nodes"][0]["node_id"]})).await.unwrap();
        assert_eq!(layout["layout"]["hit"]["kind"],"indeterminate","{layout}");assert_eq!(layout["coverage"]["truncated"],true);assert!(layout["coverage"]["limits"].as_array().unwrap().iter().any(|v|v=="depth_limit"),"{layout}");
        f.registry.clear();
        f.fixture("(()=>{document.body.replaceChildren();for(const group of ['a','b','c'])for(let i=0;i<64;i++){const e=document.body.appendChild(document.createElement('p'));e.textContent='Group-'+group+'-'+i;}return true;})()").await;
        let first=f.read(json!({"kind":"find_nodes","tag":"p","text":"Group-a-","limit":64})).await.unwrap();assert_eq!(first["nodes"].as_array().unwrap().len(),64);
        let id=first["nodes"][0]["node_id"].as_str().unwrap().to_owned();let binding=f.registry.lookup(&id,&f.port.tab_id().unwrap(),f.page.control().snapshot().0).unwrap();
        for group in ["b","c"] {let next=f.read(json!({"kind":"find_nodes","tag":"p","text":format!("Group-{group}-"),"limit":64})).await.unwrap();assert_eq!(next["nodes"].as_array().unwrap().len(),64);}
        assert_eq!(f.registry.entries.len(),128);assert!(f.registry.lookup(&id,&f.port.tab_id().unwrap(),f.page.control().snapshot().0).is_err());
        // Bypass only the public table in this internal test: the native weak
        // registry must independently have evicted the original serial.
        let query=Query::parse(&json!({"kind":"read_node","node_id":id})).unwrap();
        let error=f.page.evidence(&query,Some(binding),deadline()).await.expect_err("native old serial remained alive");assert_eq!(error.code,"browser_stale_reference");
        println!("{backend}: DOM traversal, depth, Unicode, byte and both identity-table limits passed");
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
impl Fixture {
    fn gate(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        match &self.port {
            native::Port::Cdp(p) => p.hold_next_reply(),
            native::Port::Bidi(p) => p.hold_next_reply(),
        }
    }
    fn metrics(&self) -> Value {
        match &self.port {
            native::Port::Cdp(p) => p.test_metrics(),
            native::Port::Bidi(p) => p.test_metrics(),
        }
    }
}
async fn pending_scene(backend: &'static str) {
    let mut f = Fixture::open(backend).await;
    let result=std::panic::AssertUnwindSafe(async {
        for mode in ["control","cancel","timeout","lock_deadline"] {
            let (reached,release)=f.gate();let page=f.page.clone();
            let query=Query::parse(&json!({"kind":"find_nodes","tag":"div"})).unwrap();
            let mut pending=Some(tokio::spawn(async move{page.evidence(&query,None,deadline()).await}));
            tokio::time::timeout(Duration::from_secs(2),reached).await.unwrap().unwrap();
            assert!(f.metrics()["pending"].as_u64().unwrap()>=1);assert_eq!(f.page.resources().1,0);assert_eq!(f.page.resources().2,0);
            if mode=="cancel" {let caller=pending.take().unwrap();caller.abort();assert!(caller.await.unwrap_err().is_cancelled());}
            else if mode=="timeout" {let error=pending.take().unwrap().await.unwrap().expect_err("timed read succeeded");assert!(matches!(error.code,"browser_timeout_unknown"|"browser_deadline"));}
            else if mode=="lock_deadline" {let query=Query::parse(&json!({"kind":"find_nodes","tag":"div"})).unwrap();let error=f.page.evidence(&query,None,Instant::now()+Duration::from_millis(30)).await.expect_err("locked read succeeded");assert_eq!(error.code,"browser_busy");}
            let permit=f.page.claim_human(Instant::now()+Duration::from_millis(100)).await.unwrap();assert!(f.metrics()["pending"].as_u64().unwrap()>=1);
            release.send(()).expect("native reply ownership was dropped");
            if let Some(pending)=pending {let error=pending.await.unwrap().expect_err("old-epoch evidence disclosed");assert!(matches!(error.code,"browser_control_changed"|"browser_timeout_unknown"),"{error:?}");}
            let until=deadline();while f.metrics()["pending"]!=0 {assert!(Instant::now()<until,"late read not settled: {}",f.metrics());tokio::time::sleep(Duration::from_millis(2)).await;}
            assert!(f.registry.entries.is_empty());f.page.return_to_ai(&permit,true,deadline()).await.unwrap();
        }
        println!("{backend}: cancelled/timed reads keep native ownership and cannot publish across control change");
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_dom_evidence_bounds_cdp() {
    bounded_scene("cdp").await;
}
#[tokio::test]
#[ignore = "requires native Firefox and namespace helper"]
async fn native_dom_evidence_bounds_bidi() {
    bounded_scene("bidi").await;
}
#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_dom_evidence_pending_cdp() {
    pending_scene("cdp").await;
}
#[tokio::test]
#[ignore = "requires native Firefox and namespace helper"]
async fn native_dom_evidence_pending_bidi() {
    pending_scene("bidi").await;
}
