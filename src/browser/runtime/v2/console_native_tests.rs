use super::*;
use sem::task_events::Sink;

fn sink(f: &Fixture, console: bool, network: bool) -> Sink {
    let control = f.page.control();
    Sink::new(control.clone(), control.snapshot().0, console, network)
}

async fn settled(f: &Fixture, sessions: u64) {
    let until = deadline();
    loop {
        let m = f.metrics();
        if m["sessions"]["count"] == sessions
            && m["pending"] == 0
            && m["native_objects"] == 0
            && m["leases"]["objects"] == 0
        {
            return;
        }
        assert!(
            Instant::now() < until,
            "diagnostic resources did not settle: {m}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn check_overload(script: &str) {
    let mut f = Fixture::open("cdp").await;
    let result = std::panic::AssertUnwindSafe(async {
        let root_sessions = f.metrics()["sessions"]["count"].as_u64().unwrap();
        f.fixture("(()=>{document.body.innerHTML='<button>Apply</button>'; globalThis.n=0; document.querySelector('button').onclick=()=>++n; return true})()").await;
        let capture = sink(&f, true, false);
        f.port.enable_task_events(capture.clone(), deadline()).await.unwrap();
        assert_eq!(f.metrics()["sessions"]["count"],root_sessions+2);
        f.fixture(script).await;
        let until = deadline();
        while capture.active() {
            assert!(Instant::now() < until, "failed capture remained active");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(capture.page(0, None, 64).unwrap_err().code,"browser_capture_unavailable");
        settled(&f,root_sessions).await;
        assert_eq!(f.fixture("(()=>{document.querySelector('button').click();return n})()").await,1);
        let network = sink(&f,false,true);
        f.port.enable_task_events(network.clone(),deadline()).await.unwrap();
        assert!(network.active());
        network.stop();
        let id = f.port.new_tab(deadline()).await.unwrap();
        let new_page = f.port.for_tab(&id).unwrap();
        let fresh = sink(&f,true,false);
        new_page.enable_task_events(fresh.clone(),deadline()).await.unwrap();
        fresh.stop();
        new_page.close_tab(deadline()).await.unwrap();
        settled(&f,root_sessions).await;
        assert_eq!(f.fixture("n").await,1);
    }).catch_unwind().await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn oversized_runtime_string_retires_only_the_diagnostic_window() {
    check_overload("(()=>{console.log('x'.repeat(3*1024*1024));return true})()").await;
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn excessive_remote_objects_retire_only_the_diagnostic_window() {
    check_overload(
        "(()=>{console.log(...Array.from({length:17},()=>({example:true})));return true})()",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn a_thousand_primitive_events_keep_the_exact_ring_and_stop_boundary() {
    let mut f = Fixture::open("cdp").await;
    let result = std::panic::AssertUnwindSafe(async {
        let root_sessions = f.metrics()["sessions"]["count"].as_u64().unwrap();
        let capture = sink(&f, true, false);
        f.port
            .enable_task_events(capture.clone(), deadline())
            .await
            .unwrap();
        f.fixture("(()=>{for(let i=0;i<1000;i++)console.info(i);return true})()")
            .await;
        let until = deadline();
        loop {
            let page = capture.page(0, None, 64).unwrap();
            if page["through_sequence"] == 1000 {
                break;
            }
            assert!(
                Instant::now() < until,
                "received only {}",
                page["through_sequence"]
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        capture.stop();
        settled(&f, root_sessions).await;
        let page = capture.page(0, None, 64).unwrap();
        assert_eq!(page["dropped_count"], 872);
        assert_eq!(page["events"][0]["sequence"], 873);
        assert_eq!(page["events"][0]["level"], "info");
        assert_eq!(capture.event(1000).unwrap()["level"], "info");
        assert_eq!(page["state"], "stopped");
    })
    .catch_unwind()
    .await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[tokio::test]
#[ignore = "requires native Chromium"]
async fn cancelled_console_start_retires_native_sessions_after_human_claim() {
    let mut f = Fixture::open("cdp").await;
    let result = std::panic::AssertUnwindSafe(async {
        let baseline = f.metrics()["sessions"]["count"].as_u64().unwrap();
        let native::Port::Cdp(port) = &f.port else {
            unreachable!()
        };
        let (reached, release) = port.hold_console_enable();
        let capture = sink(&f, true, false);
        let old = capture.clone();
        let port = f.port.clone();
        let pending =
            tokio::spawn(async move { port.enable_task_events(capture, deadline()).await });
        tokio::time::timeout_at(deadline().into(), reached)
            .await
            .unwrap()
            .unwrap();
        let human = f.page.claim_human(deadline()).await.unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        settled(&f, baseline).await;
        assert_eq!(
            old.page(0, None, 64).unwrap_err().code,
            "browser_capture_unavailable"
        );
        f.page.return_to_ai(&human, true, deadline()).await.unwrap();
        let fresh = sink(&f, true, false);
        f.port
            .enable_task_events(fresh.clone(), deadline())
            .await
            .unwrap();
        assert_eq!(fresh.page(0, None, 64).unwrap()["through_sequence"], 0);
        fresh.stop();
        settled(&f, baseline).await;
    })
    .catch_unwind()
    .await;
    f.native.close().await.unwrap();
    std::fs::remove_dir_all(&f.directory).unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
