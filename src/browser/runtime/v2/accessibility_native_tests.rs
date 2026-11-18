use super::*;
fn forbidden_keys(value: &Value, keys: &[Value]) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                assert!(!keys.iter().any(|v| v == key), "forbidden key: {key}");
                forbidden_keys(value, keys);
            }
        }
        Value::Array(values) => {
            for value in values {
                forbidden_keys(value, keys);
            }
        }
        _ => (),
    }
}
async fn settled(f: &Fixture) {
    let until = deadline();
    loop {
        let m = f.metrics();
        if m["native_objects"] == 0
            && m["leases"]["objects"] == 0
            && m["leases"]["reserved_objects"] == 0
            && m["pending"] == 0
        {
            return;
        }
        assert!(
            Instant::now() < until,
            "native AX resources did not settle: {m}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}
#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_accessibility_cdp() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|| async {
            axum::response::Html(include_str!(
                "../../../../tests/fixtures/native-ax-v1/page.html"
            ))
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut f = Fixture::open("cdp").await;
    let result=std::panic::AssertUnwindSafe(async {
        f.port.navigate(&origin,deadline()).await.unwrap();
        for _ in 0..100 {if f.fixture("window.axReady===true").await==true {break;}tokio::time::sleep(Duration::from_millis(10)).await;}
        assert_eq!(f.fixture("window.axReady===true").await,true);
        let observation=f.page.observe(sem::Query::default(),1000,64000,None,deadline()).await.unwrap();
        let reference=observation.rows.iter().find(|v|v.facts.name=="Native action").unwrap().reference.clone().unwrap();
        let witness=f.fixture("({events:window.axWitness,x:scrollX,y:scrollY,focus:document.activeElement.id})").await;
        let oracle:Value=serde_json::from_str(include_str!("../../../../tests/fixtures/native-ax-v1/oracle.json")).unwrap();let mut first=None;
        for case in oracle["cases"].as_array().unwrap() {
            let mut query=case["query"].clone();query["kind"]="find_nodes".into();let found=f.read(query).await.unwrap();assert_eq!(found["nodes"].as_array().unwrap().len(),1,"{found}");
            let id=found["nodes"][0]["node_id"].clone();if first.is_none(){first=Some(id.clone());}
            let value=f.read(json!({"kind":"read_accessibility","node_id":id})).await.unwrap();assert_eq!(value["source"],"native_ax");assert_eq!(value["coverage"]["native_ax_nodes"],1);
            for (field,expected) in case["expected"].as_object().unwrap() {assert_eq!(&value["accessibility"][field],expected,"{value}");}
            if let Some(states)=case["states"].as_object() {for (field,expected) in states {assert_eq!(&value["accessibility"]["states"][field],expected,"{value}");}}
            if let Some(reasons)=case["ignored_reasons_include"].as_array() {for reason in reasons {assert!(value["accessibility"]["ignored_reasons"].as_array().unwrap().contains(reason),"{value}");}}
            for text in oracle["forbidden_values"].as_array().unwrap() {assert!(!value.to_string().contains(text.as_str().unwrap()),"{value}");}
            forbidden_keys(&value,oracle["forbidden_keys"].as_array().unwrap());assert!(serde_json::to_vec(&value).unwrap().len()<=dom::MAX_BYTES);settled(&f).await;
        }
        for _ in 0..64 {let value=f.read(json!({"kind":"read_accessibility","node_id":first})).await.unwrap();assert_eq!(value["accessibility"]["role"],"button");settled(&f).await;}
        assert_eq!(witness,f.fixture("({events:window.axWitness,x:scrollX,y:scrollY,focus:document.activeElement.id})").await);
        f.page.diagnose(reference,sem::actionability::ActionKind::Click,deadline()).await.unwrap();
        for deep in [false,true] {
            f.fixture(&format!("(()=>{{document.body.replaceChildren();const root=document.body.appendChild(document.createElement('button'));root.setAttribute('aria-label','Bounded AX');let e=root;for(let i=0;i<{};i++){{const child=document.createElement('span');if({deep})child.textContent='x';e.appendChild(child);if({deep})e=child;}}return true;}})()",if deep{100}else{6000})).await;
            let found=f.read(json!({"kind":"find_nodes","tag":"button","limit":1})).await.unwrap();let value=f.read(json!({"kind":"read_accessibility","node_id":found["nodes"][0]["node_id"]})).await.unwrap();assert_eq!(value["coverage"]["native_ax_nodes"],1);assert_eq!(value["accessibility"]["name"],"Bounded AX");assert!(value["coverage"]["limits"].as_array().unwrap().iter().any(|v|v==if deep{"depth_limit"}else{"element_limit"}),"{value}");settled(&f).await;
        }
        println!("cdp: native AX oracle, name disclosure, one-node scope, preserved refs and 64 settled wrapper lifetimes passed");
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
#[tokio::test]
#[ignore = "requires native Chromium"]
async fn native_accessibility_pending_cdp() {
    let mut f = Fixture::open("cdp").await;
    let result=std::panic::AssertUnwindSafe(async {
        f.fixture("(()=>{document.body.replaceChildren();const b=document.body.appendChild(document.createElement('button'));b.textContent='AX pending';return true;})()").await;
        for mode in ["cancel","timeout","control"] {
            let found=f.read(json!({"kind":"find_nodes","tag":"button"})).await.unwrap();let id=found["nodes"][0]["node_id"].as_str().unwrap();let binding=f.registry.lookup(id,&f.port.tab_id().unwrap(),f.page.control().snapshot().0).unwrap();
            let query=Query::parse(&json!({"kind":"read_accessibility","node_id":id})).unwrap();let page=f.page.clone();
            let (reached,release)=match &f.port {native::Port::Cdp(p)=>p.hold_accessibility_reply(),_=>unreachable!()};
            let mut pending=Some(tokio::spawn(async move{page.evidence(&query,Some(binding),deadline()).await}));
            tokio::time::timeout(Duration::from_secs(2),reached).await.unwrap().unwrap();assert_eq!(f.metrics()["leases"]["objects"],1);assert_eq!(f.metrics()["native_objects"],1);
            if mode=="cancel" {let task=pending.take().unwrap();task.abort();assert!(task.await.unwrap_err().is_cancelled());}
            else if mode=="timeout" {let error=pending.take().unwrap().await.unwrap().expect_err("AX timeout disclosed a result");assert!(matches!(error.code,"browser_timeout_unknown"|"browser_deadline"));}
            let permit=f.page.claim_human(Instant::now()+Duration::from_millis(100)).await.unwrap();assert_eq!(f.metrics()["leases"]["objects"],1,"wrapper charge disappeared before release reply");
            release.send(()).unwrap();if let Some(task)=pending {let error=task.await.unwrap().expect_err("old-control AX disclosed");assert!(matches!(error.code,"browser_control_changed"|"browser_timeout_unknown"),"{error:?}");}
            settled(&f).await;f.page.return_to_ai(&permit,true,deadline()).await.unwrap();
        }
        println!("cdp: native AX cancellation and timeout retain consumed wrapper ownership until cleanup acknowledgment");
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
#[tokio::test]
#[ignore = "requires native Firefox and namespace helper"]
async fn native_accessibility_bidi_refuses_native_claims() {
    let mut f = Fixture::open("bidi").await;
    let result=std::panic::AssertUnwindSafe(async {
        f.fixture("(()=>{const b=document.body.appendChild(document.createElement('button'));b.textContent='DOM only';return true;})()").await;
        let found=f.read(json!({"kind":"find_nodes","tag":"button"})).await.unwrap();let error=f.read(json!({"kind":"read_accessibility","node_id":found["nodes"][0]["node_id"]})).await.unwrap_err();assert_eq!(error.code,"browser_feature_unsupported");
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
