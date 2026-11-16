use super::super::{
    Command, ContextId, Event,
    wire::{ProtocolError, Settlement},
};
use super::*;
use crate::{
    browser::{Failure, control::Control},
    profile_lock::ProfileLocks,
    storage,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
};

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    helper: PathBuf,
    executable: PathBuf,
    lock: Option<ProfileLocks>,
    evidence: fs::File,
}
impl Fixture {
    fn new(label: &str) -> Self {
        let destination = PathBuf::from(
            std::env::var_os("RDH_D2B_EVIDENCE").expect("explicit evidence directory"),
        );
        let base = destination.join(format!(
            "channel-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        storage::create_private_dir(&base).unwrap();
        let root = base.join(".rdh-browser");
        storage::create_private_dir(&root).unwrap();
        let lock = Some(ProfileLocks::acquire(&[crate::browser::storage_lock(&root)]).unwrap());
        Self {
            base,
            root,
            helper: PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap()),
            executable: PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap()),
            lock,
            evidence: fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination.join(format!("{label}.jsonl")))
                .unwrap(),
        }
    }
    async fn open(&self) -> Managed {
        let session = Managed::open(&self.helper, &self.executable, &self.root)
            .await
            .unwrap();
        let boot = session.boot.as_ref().unwrap();
        assert_eq!(boot.product, "firefox");
        assert_eq!(boot.version, std::env::var("RDH_FIREFOX_VERSION").unwrap());
        assert_eq!(boot.contexts.ids().len(), 1);
        session
    }
    fn record(&mut self, value: Value) {
        writeln!(self.evidence, "{value}").unwrap();
        self.evidence.flush().unwrap();
    }
    fn finish(mut self) {
        crate::browser::process::recover(&self.root).unwrap();
        assert_eq!(fs::read_dir(&self.root).unwrap().count(), 0);
        self.lock.take().unwrap().release().unwrap();
        let mut lock = ProfileLocks::acquire(&[crate::browser::storage_lock(&self.root)]).unwrap();
        lock.release().unwrap();
        self.record(json!({"kind":"finished","directories_remaining":0,"lock_reacquired":true}));
        fs::remove_dir_all(&self.base).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("D2b channel fixture retained: {}", self.base.display());
        }
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn rss() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        * 1024
}
async fn context_event(session: &mut Managed, context: &ContextId, created: bool) {
    let event = tokio::time::timeout(
        Duration::from_secs(5),
        session.events.as_mut().unwrap().recv(),
    )
    .await
    .unwrap()
    .unwrap();
    match event {
        Event::Created {
            context: actual, ..
        } if created => assert_eq!(&actual, context),
        Event::Destroyed { context: actual } if !created => assert_eq!(&actual, context),
        other => panic!("unexpected lifecycle event {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native Firefox and private evidence"]
async fn lifecycle_and_resource_cycles() {
    let mut fixture = Fixture::new("channel-cycles");
    let cycles: usize = std::env::var("RDH_D2B_CYCLES")
        .unwrap_or_else(|_| "100".into())
        .parse()
        .unwrap();
    for iteration in 0..cycles {
        let started = Instant::now();
        let mut session = fixture.open().await;
        let startup_ms = started.elapsed().as_secs_f64() * 1000.;
        let result = session
            .channel
            .call(Command::CreateTab, deadline())
            .await
            .unwrap()
            .value;
        let context = session.channel.context(&result["context"]).unwrap();
        context_event(&mut session, &context, true).await;
        session
            .channel
            .call(Command::CloseContext(context.clone()), deadline())
            .await
            .unwrap();
        context_event(&mut session, &context, false).await;
        assert_eq!(session.channel.snapshot()["pending"], 0);
        assert_eq!(session.channel.snapshot()["event_bytes"], 0);
        let started = Instant::now();
        session.close(true).await.unwrap();
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
        let resources = session.channel.snapshot();
        assert_eq!(resources["pending"], 0);
        assert_eq!(resources["event_bytes"], 0);
        assert_eq!(resources["workers"], 0);
        fixture.record(json!({"kind":"cycle","iteration":iteration,"startup_ms":startup_ms,"close_ms":started.elapsed().as_secs_f64()*1000.,"resources":resources,"agent_rss_bytes":rss(),"directories_remaining":0}));
    }
    fixture.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "holds real Firefox replies at an explicit transport barrier"]
async fn control_timeout_cancellation_and_late_receipts() {
    let mut fixture = Fixture::new("channel-control-late");
    for iteration in 0..20 {
        let mut session = fixture.open().await;
        let original = session.boot.as_ref().unwrap().contexts.ids()[0].clone();
        let control = Arc::new(Control::new());
        for case in [
            "timeout_success",
            "cancel_success",
            "timeout_error",
            "cancel_error",
        ] {
            let success = case.ends_with("success");
            let timeout = case.starts_with("timeout");
            let (epoch, paused) = control.snapshot();
            assert!(!paused);
            let (client, effects) = session
                .channel
                .authorized(control.clone(), epoch, false)
                .track_effects();
            let (reached, release) = session.channel.hold_next_reply();
            let command = if success {
                Command::CreateTab
            } else {
                Command::CloseContext(
                    session
                        .channel
                        .context(&json!("d2b-does-not-exist"))
                        .unwrap(),
                )
            };
            let call = client
                .submit(command, Instant::now() + Duration::from_millis(300))
                .unwrap();
            let receipt = call.receipt.clone();
            tokio::time::timeout(Duration::from_secs(5), reached)
                .await
                .unwrap()
                .unwrap();
            if timeout {
                assert!(matches!(call.wait().await, Err(Error::TimeoutUnknown)));
            } else {
                drop(call);
            }
            assert_eq!(receipt.status(), Settlement::Enqueued);
            assert_eq!(session.channel.pending_effects(), 1);
            assert!(effects.load(Ordering::Acquire));
            let started = Instant::now();
            let human_epoch = control.change(true);
            let fence_ms = started.elapsed().as_secs_f64() * 1000.;
            assert!(matches!(
                client.submit(Command::CreateTab, deadline()),
                Err(Error::Rejected(Failure {
                    code: "browser_control_changed",
                    state: "not_sent"
                }))
            ));
            release.send(()).unwrap();
            let human = session
                .channel
                .authorized(control.clone(), human_epoch, true);
            human.call(Command::Status, deadline()).await.unwrap();
            let expected = if success {
                Settlement::Success
            } else {
                Settlement::ProtocolError(ProtocolError::NoSuchFrame)
            };
            assert_eq!(receipt.status(), expected);
            assert_eq!(session.channel.pending_effects(), 0);
            assert!(effects.load(Ordering::Acquire));
            let tree = human.call(Command::Tree, deadline()).await.unwrap().value;
            let contexts = tree["contexts"].as_array().unwrap();
            assert_eq!(contexts.len(), if success { 2 } else { 1 });
            if success {
                let context = contexts
                    .iter()
                    .map(|v| session.channel.context(&v["context"]).unwrap())
                    .find(|id| id != &original)
                    .unwrap();
                context_event(&mut session, &context, true).await;
                human
                    .call(Command::CloseContext(context.clone()), deadline())
                    .await
                    .unwrap();
                context_event(&mut session, &context, false).await;
            }
            control.resume(human_epoch).unwrap();
            fixture.record(json!({"kind":"late_receipt","iteration":iteration,"case":case,"receipt":format!("{expected:?}"),"fence_ms":fence_ms,"barrier_before_reply":1,"barrier_after_reply":0,"effects_still_possible":true,"context_count":contexts.len(),"replay_count":0,"resources":session.channel.snapshot()}));
        }
        session.close(true).await.unwrap();
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    }
    fixture.finish();
}

async fn completion(receiver: &mut watch::Receiver<Completion>) {
    tokio::time::timeout(Duration::from_millis(3500), async {
        while *receiver.borrow_and_update() == Completion::Running {
            receiver.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(*receiver.borrow() == Completion::Stopped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "verifies independent native cleanup after drop and unconsumed event overflow"]
async fn caller_drop_and_critical_gap_automatically_close_the_native_owner() {
    let mut fixture = Fixture::new("channel-automatic-cleanup");
    for iteration in 0..5 {
        let session = fixture.open().await;
        let channel = session.channel.clone();
        let mut done = session.completion.clone();
        drop(session);
        completion(&mut done).await;
        channel.wait_stopped(deadline()).await.unwrap();
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
        fixture.record(json!({"kind":"caller_drop","iteration":iteration,"resources":channel.snapshot(),"directories_remaining":0}));
    }
    let mut session = fixture.open().await;
    let delivered = session.events.as_ref().unwrap().delivered();
    let mut iterations = 0;
    while session.channel.connected() && iterations < 130 {
        iterations += 1;
        let result = session.channel.call(Command::CreateTab, deadline()).await;
        let Ok(result) = result else {
            break;
        };
        let context = session.channel.context(&result.value["context"]).unwrap();
        if session
            .channel
            .call(Command::CloseContext(context), deadline())
            .await
            .is_err()
        {
            break;
        }
    }
    assert!(!session.channel.connected());
    assert_eq!(session.events.as_ref().unwrap().delivered(), delivered);
    assert!(matches!(
        *session.channel.lifecycle().borrow(),
        Mode::Stopped(Fault::EventGap)
    ));
    completion(&mut session.completion).await;
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    session.close(false).await.unwrap();
    fixture.record(json!({"kind":"critical_event_gap","context_cycles":iterations,"consumer_received_events":0,"directories_remaining":0,"resources":session.channel.snapshot()}));
    fixture.finish();
}
