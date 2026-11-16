use super::*;

#[tokio::test]
async fn full_admission_and_critical_event_backlog_do_not_delay_control_fences() {
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let (channel, events, mut browser) = fixture().await;
        let control = Arc::new(Control::new());
        let old = channel.authorized(control.clone(), 1, false);
        // The peer never reads requests: the first write blocks, and all 32
        // admission slots remain owned while a burst loads the event path.
        drop(
            old.submit(
                Command::TestPayload {
                    effect: Effect::Page,
                    text: "x".repeat(200 * 1024),
                },
                deadline(),
            )
            .unwrap(),
        );
        for _ in 1..SLOTS {
            drop(old.submit(Command::Status, deadline()).unwrap());
        }
        assert_eq!(channel.snapshot()["free_slots"], 0);
        for _ in 0..128 {
            event(&mut browser, "load").await;
        }
        while channel.inner.event_sequence.load(Ordering::Acquire) != 128 {
            tokio::task::yield_now().await;
        }
        assert_eq!(events.queue.len(), 128);
        assert_eq!(channel.snapshot()["pending"], SLOTS);
        assert!(channel.outgoing.capacity() <= 1);
        let started = Instant::now();
        control.change(true);
        let refusal = old
            .submission
            .enqueue::<()>(Effect::Page, || {
                panic!("revoked control reached the submission closure")
            })
            .unwrap_err();
        assert_eq!(
            (refusal.code, refusal.state),
            ("browser_control_changed", "not_sent")
        );
        samples.push(started.elapsed().as_nanos() as u64);
        // The public adapter may refuse capacity before checking authorization;
        // either way this command cannot enter the full pending table.
        assert!(matches!(
            old.submit(Command::CreateTab, deadline()),
            Err(Error::Rejected(Failure {
                code: "browser_busy",
                state: "not_sent"
            }))
        ));
        assert_eq!(channel.snapshot()["pending"], SLOTS);
        assert_eq!(channel.pending_effects(), 1);
        finish(&channel, events).await;
    }
    let mut ordered = samples.clone();
    ordered.sort_unstable();
    let p99 = ordered[989];
    let report = json!({"scenario":"control_fence_under_pending_and_event_load",
        "samples_ns":samples,"sample_count":1000,"pending_per_sample":32,
        "unconsumed_events_per_sample":128,"outgoing_queue_minimum":31,
        "p99_ns":p99,"max_ns":ordered[999],"limit_ns":100_000_000,
        "scope":"in-memory WebSocket fixture; admission fence only"});
    if let Some(root) = std::env::var_os("RDH_D2B_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(root).join("control-fence-latency.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
    assert!(
        p99 <= 100_000_000,
        "control fence p99 {p99} ns exceeds 100 ms"
    );
}
