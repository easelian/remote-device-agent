//! Deterministic queue/fence tests: the endpoint controls responses and Tokio
//! controls deadlines. No sleep is used to guess whether a command was sent.
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

fn fixture() -> (Cdp, BufReader<DuplexStream>) {
    let (client, browser) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    {
        let cdp = Cdp::new(read, write);
        cdp.test_sessions(&["page"]);
        (cdp, BufReader::new(browser))
    }
}

fn deadline() -> Instant {
    (tokio::time::Instant::now() + Duration::from_secs(30)).into_std()
}

async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
    let mut frame = Vec::new();
    browser.read_until(0, &mut frame).await.unwrap();
    assert_eq!(frame.pop(), Some(0));
    serde_json::from_slice(&frame).unwrap()
}

async fn send(browser: &mut BufReader<DuplexStream>, value: Value) {
    let mut frame = serde_json::to_vec(&value).unwrap();
    frame.push(0);
    browser.get_mut().write_all(&frame).await.unwrap();
}

// The reserved id-zero response proves all preceding replies were processed;
// this needs neither a command slot nor a temporary essential event consumer.
async fn processed(cdp: &Cdp, browser: &mut BufReader<DuplexStream>) {
    let previous = cdp.inner.barriers.load(Ordering::Acquire);
    send(browser, json!({"id":0,"result":{}})).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while cdp.inner.barriers.load(Ordering::Acquire) == previous {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn call(cdp: Cdp, command: Command, params: Value) -> tokio::task::JoinHandle<Result<Value>> {
    tokio::spawn(async move { cdp.call(Some("page"), command, params, deadline()).await })
}

#[tokio::test(start_paused = true)]
async fn enqueued_effect_survives_fence_timeout_and_late_success() {
    let (cdp, mut browser) = fixture();
    let control = Arc::new(Control::new());
    let (authorized, effects) = cdp.authorized(control.clone(), 1).track_effects();
    let task = call(
        authorized.clone(),
        Command::InputInsertText,
        json!({"text":"fixture"}),
    );
    let sent = request(&mut browser).await;
    assert_eq!(sent["method"], "Input.insertText");
    assert_eq!(sent["params"], json!({"text":"fixture"}));
    assert_eq!(sent["sessionId"], "page");
    assert_eq!(cdp.pending(), 1);
    assert!(effects.load(Ordering::Acquire));
    control.change(true);
    let refused = authorized
        .call(Some("page"), Command::DomFocus, json!({}), deadline())
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code, refused.state),
        ("browser_control_changed", "not_sent")
    );
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_timeout_unknown", "unknown")
    );
    assert_eq!(cdp.pending(), 1);
    assert_eq!(cdp.inner.slots.available_permits(), 31);
    send(&mut browser, json!({"id":sent["id"],"result":{}})).await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.pending(), 0);
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    assert!(effects.load(Ordering::Acquire));
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn cancelled_reads_keep_transport_ownership_without_an_input_barrier() {
    let (cdp, mut browser) = fixture();
    let (tracked, effects) = cdp.track_effects();
    let task = call(tracked, Command::RuntimeEvaluate(Effect::None), json!({}));
    let sent = request(&mut browser).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(cdp.pending(), 0);
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 1);
    assert_eq!(cdp.inner.slots.available_permits(), 31);
    assert!(!effects.load(Ordering::Acquire));
    send(
        &mut browser,
        json!({"id":sent["id"],"error":{"code":-32000}}),
    )
    .await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 0);
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn script_effects_and_late_error_ids_do_not_leak_between_operations() {
    let (cdp, mut browser) = fixture();
    let (first, effects) = cdp.track_effects();
    let task = call(first, Command::RuntimeCallFunction(Effect::Page), json!({}));
    let old = request(&mut browser).await;
    assert_eq!(old["method"], "Runtime.callFunctionOn");
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(task.await.unwrap().unwrap_err().state, "unknown");
    assert_eq!(cdp.pending(), 1);
    let (second, new_effects) = cdp.track_effects();
    let next_task = call(
        second,
        Command::RuntimeCallFunction(Effect::None),
        json!({}),
    );
    let next = request(&mut browser).await;
    assert_ne!(old["id"], next["id"]);
    send(
        &mut browser,
        json!({"id":old["id"],"error":{"code":-32000}}),
    )
    .await;
    // A duplicate old reply must not consume or answer the new request.
    send(
        &mut browser,
        json!({"id":old["id"],"result":{"wrong":true}}),
    )
    .await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.pending(), 0);
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 1);
    assert!(!next_task.is_finished());
    assert!(effects.load(Ordering::Acquire));
    assert!(!new_effects.load(Ordering::Acquire));
    send(
        &mut browser,
        json!({"id":next["id"],"result":{"fresh":true}}),
    )
    .await;
    assert_eq!(next_task.await.unwrap().unwrap(), json!({"fresh":true}));
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn saturated_slots_refuse_new_effects_but_preserve_cancelled_work() {
    let (cdp, mut browser) = fixture();
    let mut sent = Vec::new();
    for _ in 0..32 {
        let task = call(cdp.clone(), Command::PageGetFrameTree, json!({}));
        sent.push(request(&mut browser).await);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(cdp.inner.slots.available_permits(), 0);
    assert_eq!(cdp.pending(), 0);
    let (tracked, effects) = cdp.track_effects();
    let error = tracked
        .call(
            Some("page"),
            Command::InputInsertText,
            json!({"text":"fixture"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    assert!(!effects.load(Ordering::Acquire));
    assert_eq!(cdp.inner.pending.lock().unwrap().commands.len(), 32);
    for command in sent {
        send(&mut browser, json!({"id":command["id"],"result":{}})).await;
    }
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn failed_enqueue_rolls_back_pending_and_does_not_mark_an_effect() {
    let (mut cdp, _browser) = fixture();
    let _keep_writer_alive = cdp.outgoing.clone();
    let (queue, mut requests) = mpsc::channel(1);
    queue.try_send(vec![7]).unwrap();
    cdp.outgoing = queue;
    let (tracked, effects) = cdp.track_effects();
    let error = tracked
        .call(
            Some("page"),
            Command::PageNavigate,
            json!({"url":"about:blank"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!((error.code, error.state), ("browser_busy", "not_sent"));
    assert_eq!(cdp.pending(), 0);
    assert!(cdp.inner.pending.lock().unwrap().commands.is_empty());
    assert_eq!(cdp.inner.slots.available_permits(), 32);
    assert!(!effects.load(Ordering::Acquire));
    assert_eq!(requests.try_recv().unwrap(), vec![7]);
    assert!(requests.try_recv().is_err());
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn dialog_blocks_new_input_but_keeps_release_ownership_until_reply() {
    let (cdp, mut browser) = fixture();
    let _events = cdp.events();
    send(
        &mut browser,
        json!({"sessionId":"page","method":"Page.javascriptDialogOpening","params":{}}),
    )
    .await;
    processed(&cdp, &mut browser).await;
    let (tracked, effects) = cdp.track_effects();
    let error = tracked
        .call(
            Some("page"),
            Command::InputKeyPress,
            json!({"type":"keyDown","key":"Enter"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_dialog_open", "not_sent")
    );
    assert_eq!(cdp.pending(), 0);
    assert!(!effects.load(Ordering::Acquire));
    let error = tracked
        .cleanup()
        .call(
            Some("page"),
            Command::InputKeyRelease,
            json!({"type":"keyUp","key":"Enter"}),
            deadline(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.code, error.state),
        ("browser_dialog_open", "unknown")
    );
    let release = request(&mut browser).await;
    assert_eq!(release["method"], "Input.dispatchKeyEvent");
    assert_eq!(release["params"]["type"], "keyUp");
    assert_eq!(cdp.pending(), 1);
    assert!(effects.load(Ordering::Acquire));
    send(&mut browser, json!({"id":release["id"],"result":{}})).await;
    processed(&cdp, &mut browser).await;
    assert_eq!(cdp.pending(), 0);
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn paired_input_releases_its_press_after_control_changes() {
    let (cdp, mut browser) = fixture();
    let control = Arc::new(Control::new());
    let (tracked, effects) = cdp.authorized(control.clone(), 1).track_effects();
    let task = tokio::spawn(async move {
        tracked
            .input_pair(
                Some("page"),
                InputPair::Key,
                json!({"type":"keyDown","key":"Enter"}),
                json!({"type":"keyUp","key":"Enter"}),
                deadline(),
            )
            .await
    });
    let press = request(&mut browser).await;
    assert_eq!(press["params"]["type"], "keyDown");
    control.change(true);
    send(&mut browser, json!({"id":press["id"],"result":{}})).await;
    let release = request(&mut browser).await;
    assert_eq!(release["params"]["type"], "keyUp");
    assert_eq!(release["method"], press["method"]);
    assert_ne!(release["id"], press["id"]);
    assert_eq!(cdp.pending(), 1);
    send(&mut browser, json!({"id":release["id"],"result":{}})).await;
    task.await.unwrap().unwrap();
    assert_eq!(cdp.pending(), 0);
    assert!(effects.load(Ordering::Acquire));
    cdp.stop();
}

#[tokio::test(start_paused = true)]
async fn disconnect_does_not_erase_committed_operation_progress() {
    let (cdp, mut browser) = fixture();
    let (tracked, effects) = cdp.track_effects();
    let task = call(tracked, Command::PageNavigate, json!({"url":"about:blank"}));
    request(&mut browser).await;
    drop(browser);
    let error = task.await.unwrap().unwrap_err();
    assert_eq!((error.code, error.state), ("browser_lost", "unknown"));
    assert!(!cdp.connected());
    assert!(effects.load(Ordering::Acquire));
    assert_eq!(cdp.inner.slots.available_permits(), 32);
}

#[tokio::test(start_paused = true)]
async fn ten_thousand_bounded_schedules_preserve_pending_fences_and_no_replay() {
    let mut coverage = std::collections::BTreeMap::<&str, usize>::new();
    let mut random = 0x63d1_bc20_2611_1601u64;
    let mut max_steps = 0;
    for sequence in 0..10_000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let bits = random;
        let mut steps = 0;
        let (cdp, mut browser) = fixture();
        let control = Arc::new(Control::new());
        let old = cdp.authorized(control.clone(), 1);
        if bits & 1 != 0 {
            control.change(true);
            steps += 1;
            let error = old
                .call(
                    Some("page"),
                    Command::InputInsertText,
                    json!({"text":"fixture"}),
                    deadline(),
                )
                .await
                .unwrap_err();
            steps += 1;
            assert_eq!(error.code, "browser_control_changed");
            *coverage.entry("fence_before_submit").or_default() += 1;
        }
        let (epoch, paused) = control.snapshot();
        let allowed = if paused {
            cdp.human(control.clone(), epoch)
        } else {
            cdp.authorized(control.clone(), epoch)
        };
        let (allowed, effects) = allowed.track_effects();
        let mut pending = Vec::new();
        let mut input = 0;
        let mut any_effect = false;
        for index in 0..(1 + (bits >> 1) % 4) {
            let effect = (bits >> (8 + index)) & 1 != 0;
            let command = if effect {
                Command::InputInsertText
            } else {
                Command::PageGetFrameTree
            };
            let task = call(allowed.clone(), command, json!({}));
            let sent = request(&mut browser).await;
            steps += 1;
            input += usize::from(effect);
            any_effect |= effect;
            pending.push((sent, Some(task), effect));
            assert_eq!(cdp.pending(), input);
            assert_eq!(
                cdp.inner.pending.lock().unwrap().commands.len(),
                pending.len()
            );
            assert_eq!(cdp.inner.slots.available_permits(), 32 - pending.len());
            assert_eq!(effects.load(Ordering::Acquire), any_effect);
            *coverage
                .entry(if effect {
                    "effect_submit"
                } else {
                    "read_submit"
                })
                .or_default() += 1;
        }
        control.change(!paused);
        steps += 1;
        let error = allowed
            .call(Some("page"), Command::DomFocus, json!({}), deadline())
            .await
            .unwrap_err();
        steps += 1;
        assert_eq!(
            (error.code, error.state),
            ("browser_control_changed", "not_sent")
        );
        assert_eq!(cdp.pending(), input);
        *coverage.entry("submit_before_fence").or_default() += 1;
        for (index, (_, task, _)) in pending.iter_mut().enumerate() {
            if (bits >> (16 + index)) & 1 != 0 {
                let task = task.take().unwrap();
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                steps += 1;
                assert_eq!(cdp.pending(), input);
                *coverage
                    .entry("caller_cancelled_pending_retained")
                    .or_default() += 1;
            }
        }
        if sequence % 13 == 0 {
            drop(browser);
            steps += 1;
            for (_, task, _) in &mut pending {
                if let Some(task) = task.take() {
                    assert_eq!(task.await.unwrap().unwrap_err().code, "browser_lost");
                }
            }
            while cdp.connected() {
                tokio::task::yield_now().await;
            }
            assert_eq!(cdp.inner.slots.available_permits(), 32);
            assert_eq!(cdp.pending(), 0);
            *coverage.entry("disconnect_settlement").or_default() += 1;
        } else {
            let timed_out = bits & 4 != 0;
            if timed_out {
                tokio::time::advance(Duration::from_secs(31)).await;
                steps += 1;
                for (_, task, _) in &mut pending {
                    if let Some(task) = task.take() {
                        let e = task.await.unwrap().unwrap_err();
                        assert_eq!((e.code, e.state), ("browser_timeout_unknown", "unknown"));
                        *coverage.entry("timeout_pending_retained").or_default() += 1;
                    }
                }
                assert_eq!(cdp.pending(), input);
                assert_eq!(cdp.inner.slots.available_permits(), 32 - pending.len());
            }
            let total = pending.len();
            for (index, (sent, task, effect)) in pending.into_iter().enumerate() {
                let failed = (bits >> (24 + index)) & 1 != 0;
                let reply = if failed {
                    json!({"id":sent["id"],"error":{"code":-32000}})
                } else {
                    json!({"id":sent["id"],"result":{"sequence":sequence}})
                };
                send(&mut browser, reply).await;
                processed(&cdp, &mut browser).await;
                steps += 1;
                input -= usize::from(effect);
                if let Some(task) = task {
                    let result = task.await.unwrap();
                    if failed {
                        assert_eq!(result.unwrap_err().code, "browser_command_failed");
                    } else {
                        assert_eq!(result.unwrap()["sequence"], sequence);
                    }
                }
                assert_eq!(cdp.pending(), input);
                assert_eq!(
                    cdp.inner.slots.available_permits(),
                    32 - (total - index - 1)
                );
                *coverage
                    .entry(match (timed_out, failed) {
                        (true, true) => "late_failure",
                        (true, false) => "late_success",
                        (false, true) => "failure",
                        (false, false) => "success",
                    })
                    .or_default() += 1;
                send(
                    &mut browser,
                    json!({"id":sent["id"],"result":{"duplicate":true}}),
                )
                .await;
                processed(&cdp, &mut browser).await;
                steps += 1;
                assert_eq!(cdp.pending(), input);
                assert_eq!(
                    cdp.inner.pending.lock().unwrap().commands.len(),
                    total - index - 1
                );
                *coverage.entry("duplicate_reply_ignored").or_default() += 1;
            }
            // The endpoint received only explicit submissions, even after timeouts,
            // cancellations, fences and duplicate responses.
            assert!(
                tokio::time::timeout(Duration::from_millis(1), request(&mut browser))
                    .await
                    .is_err()
            );
            steps += 1;
            assert_eq!(cdp.pending(), 0);
            assert_eq!(cdp.inner.slots.available_permits(), 32);
            cdp.stop();
        }
        assert_eq!(effects.load(Ordering::Acquire), any_effect);
        assert!(steps <= 64);
        max_steps = max_steps.max(steps);
    }
    for required in [
        "fence_before_submit",
        "effect_submit",
        "read_submit",
        "caller_cancelled_pending_retained",
        "disconnect_settlement",
        "timeout_pending_retained",
        "late_failure",
        "late_success",
        "failure",
        "success",
    ] {
        assert!(coverage[required] > 0);
    }
    eprintln!(
        "D1_SCHEDULES {}",
        json!({"seed":"63d1bc2026111601","sequences":10000,"max_steps":max_steps,"old_epoch_submissions":0,"unknown_replays":0,"coverage":coverage})
    );
}

#[tokio::test]
async fn thousand_fences_remain_prompt_with_full_queue_and_event_ring() {
    let (mut cdp, _browser) = fixture();
    let control = Arc::new(Control::new());
    let original = cdp.outgoing.clone();
    let (queue, _requests) = mpsc::channel(32);
    cdp.outgoing = queue;
    let mut tasks = Vec::new();
    for _ in 0..32 {
        tasks.push(call(
            cdp.authorized(control.clone(), 1),
            Command::InputInsertText,
            json!({}),
        ));
    }
    while cdp.inner.slots.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(cdp.outgoing.capacity(), 0);
    let mut events = cdp.events();
    let mut samples = Vec::new();
    for index in 0..1000 {
        for _ in 0..256 {
            let value = json!({"method":"Target.targetInfoChanged","params":{"targetInfo":{"targetId":"pressure"}}});
            let bytes = serde_json::to_vec(&value).unwrap().len();
            cdp.inner.publish_event(
                &mut cdp.inner.pending.lock().unwrap(),
                &cdp.events,
                value,
                bytes,
            );
        }
        let start = Instant::now();
        let epoch = control.change(index % 2 == 0);
        samples.push(start.elapsed().as_nanos() as u64);
        for _ in 0..256 {
            drop(events.recv().await.unwrap());
        }
        assert_eq!(cdp.test_metrics()["events"]["count"], 0);
        assert_eq!(control.snapshot().0, epoch);
        assert_eq!(cdp.pending(), 32);
        assert_eq!(cdp.outgoing.capacity(), 0);
    }
    samples.sort_unstable();
    let p99 = samples[989];
    assert!(p99 <= 100_000_000);
    eprintln!(
        "D1_FENCES {}",
        json!({"count":1000,"queue":32,"events_per_fence":256,"p99_ns":p99,"max_ns":samples[999]})
    );
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    assert_eq!(cdp.pending(), 32);
    cdp.stop();
    drop(original);
}
