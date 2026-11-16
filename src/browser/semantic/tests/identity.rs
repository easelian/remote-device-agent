use super::{
    observation::{self, Harness as _, Scene},
    *,
};

trait Harness {
    type Page: observation::Harness;
    fn port(&self) -> Self::Page;
    fn realm(&self);
    fn visit(&self, url: &str) -> impl Future<Output = Result<()>> + Send;
    fn back(&self, url: &str) -> impl Future<Output = Result<bool>> + Send;
}
macro_rules! native_harness {
    ($ty:ty,$port:ty)=>{
        impl Harness for $ty {
            type Page=$port;
            fn port(&self)->Self::Page{self.port.clone()}
            fn realm(&self){self.port.advance_semantic_realm();}
            async fn visit(&self,url:&str)->Result<()>{self.navigate(url,deadline()).await}
            async fn back(&self,url:&str)->Result<bool>{
                let started=self.port.fixture("(()=>{const at=Date.now();setTimeout(()=>history.back(),0);return at;})()",deadline()).await?;
                let expected=serde_json::to_string(url).unwrap();let until=deadline();
                loop {
                    if matches!(self.port.fixture(&format!("location.href==={expected}&&document.readyState==='complete'&&globalThis.shownAt>={started}"),until).await,Ok(value) if value==true) {
                        return self.port.fixture("globalThis.persisted===true",until).await.map(|v|v==true);
                    }
                    if Instant::now()>=until{return Err(types::stale());}
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }
}
native_harness!(
    engine::semantic_cdp::Session,
    engine::semantic_cdp::Semantic
);
#[cfg(target_os = "linux")]
native_harness!(engine::bidi::page::Session, engine::bidi::page::Semantic);
struct FakeSession(Fake);
impl Harness for FakeSession {
    type Page = Fake;
    fn port(&self) -> Fake {
        self.0.clone()
    }
    fn realm(&self) {
        let mut data = self.0.data.lock().unwrap();
        data.frame.realm += 1;
        data.frame.marker.realm.push('x');
    }
    async fn visit(&self, url: &str) -> Result<()> {
        let mut data = self.0.data.lock().unwrap();
        data.frame.document += 1;
        data.frame.realm += 1;
        data.frame.url = url.into();
        data.frame.marker.url = url.into();
        Ok(())
    }
    async fn back(&self, url: &str) -> Result<bool> {
        self.visit(url).await?;
        Ok(true)
    }
}
async fn assertions<H: Harness>(session: &H, url: &str) -> Value {
    session.visit(url).await.unwrap();
    let port = session.port();
    let page = Page::new(port.clone());
    port.scene(Scene::Records).await.unwrap();
    let before = port.frames(deadline()).await.unwrap()[0].clone();
    let query = Query {
        filter: Filter::Interactive,
        ..Default::default()
    };
    let read = page
        .observe(query.clone(), 1, 64000, None, deadline())
        .await
        .unwrap();
    let old = read.rows[0].reference.clone().unwrap();
    assert!(read.cursor.is_some());
    session.realm();
    let after = port.frames(deadline()).await.unwrap()[0].clone();
    assert_eq!(before.context, after.context);
    assert_eq!(before.document, after.document);
    assert_ne!(before.realm, after.realm);
    assert_eq!(
        page.act(old, Action::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    assert!(
        page.observe(query, 1, 64000, read.cursor, deadline())
            .await
            .is_err()
    );
    let fresh = observation::read(&page).await;
    page.act(
        observation::reference(&fresh, "a"),
        Action::Click,
        deadline(),
    )
    .await
    .unwrap();
    let stale = observation::reference(&observation::read(&page).await, "a");
    session.visit(url).await.unwrap();
    let reload = port.frames(deadline()).await.unwrap()[0].clone();
    assert_eq!(after.url, reload.url);
    assert_ne!(after.document, reload.document);
    assert_eq!(
        page.act(stale, Action::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    port.scene(Scene::Records).await.unwrap();
    let read = observation::read(&page).await;
    let stale = observation::reference(&read, "a");
    let before_history = port.frames(deadline()).await.unwrap()[0].clone();
    // Do not refresh identity while away: a real BFCache restoration can
    // return the original native document and realm keys.
    session.visit(&format!("{url}away")).await.unwrap();
    let persisted = session.back(url).await.unwrap();
    let restored = port.frames(deadline()).await.unwrap()[0].clone();
    eprintln!(
        "history identity: {}",
        json!({"persisted":persisted,"before":before_history,"after":restored})
    );
    assert_ne!(before_history.document, restored.document);
    assert_eq!(
        page.act(stale, Action::Click, deadline())
            .await
            .unwrap_err()
            .code,
        "browser_stale_reference"
    );
    port.scene(Scene::Records).await.unwrap();
    page.act(
        observation::reference(&observation::read(&page).await, "a"),
        Action::Click,
        deadline(),
    )
    .await
    .unwrap();
    json!({"real_realm_switch":true,"realm_preserves_document":true,"same_url_reload_fenced":true,"history_old_reference_fenced":true,"pageshow_persisted":persisted,"before_realm":before,"after_realm":after,"reload":reload,"before_history":before_history,"restored":restored})
}
#[tokio::test]
async fn fake_identity_uses_shared_realm_reload_and_restoration_assertions() {
    assertions(&FakeSession(Fake::new()), "http://fixture/").await;
}
#[tokio::test]
async fn strict_policy_is_rejected_before_native_allocation() {
    let root = std::env::temp_dir().join(format!(
        "rdh-semantic-admission-{}",
        crate::random::hex_token::<8>().unwrap()
    ));
    crate::storage::create_private_dir(&root).unwrap();
    let missing = root.join("missing-executable");
    let policy =
        policy::Policy::new(serde_json::from_value(json!({"private_networks":"deny"})).unwrap());
    let error = engine::semantic_cdp::Session::launch(&missing, &root, policy.clone(), deadline())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "browser_policy_unsupported");
    #[cfg(target_os = "linux")]
    {
        let error = engine::bidi::page::Session::open(&missing, &missing, &root, policy)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "browser_policy_unsupported");
    }
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Chrome and evidence directory"]
async fn cdp_native_identity() {
    let (url, server) = server().await;
    let root = root("cdp-identity");
    let browser = PathBuf::from(std::env::var_os("RDH_BROWSER_EXECUTABLE").unwrap());
    let mut session = engine::semantic_cdp::Session::launch(
        &browser,
        &root,
        policy::Policy::new(Default::default()),
        deadline(),
    )
    .await
    .unwrap();
    record("cdp-identity.json", &assertions(&session, &url).await);
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit native Firefox and evidence directory"]
async fn bidi_native_identity() {
    let (url, server) = server().await;
    let root = root("bidi-identity");
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
    record("bidi-identity.json", &assertions(&session, &url).await);
    session.close().await.unwrap();
    server.abort();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir(root).unwrap();
}
