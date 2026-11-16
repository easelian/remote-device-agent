//! Fixed-seed schedules exercise the real WebSocket transport, not a second
//! implementation of its state machine. The small oracle owns expected counts.
use super::*;
use std::collections::BTreeMap;

struct Work {
    call: Option<Call>,
    receipt: Receipt,
    effect: bool,
    epoch: u64,
}
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
fn hit(counts: &mut BTreeMap<&'static str, u64>, name: &'static str) {
    *counts.entry(name).or_default() += 1;
}

#[tokio::test(start_paused = true)]
async fn ten_thousand_bounded_schedules_preserve_ownership_and_fences() {
    const SEEDS: [u64; 4] = [
        0x3d48_1ea2_96b0_75cf,
        0xa85c_2e61_701d_94b3,
        0x9f01_6abc_425e_d873,
        0x6c30_b792_5df8_14ea,
    ];
    let mut counts = BTreeMap::new();
    let mut max_steps = 0;
    for sequence in 0..10_000u64 {
        let mut rng = SEEDS[(sequence % 4) as usize] ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let (channel, mut events, mut browser) = fixture().await;
        let control = Arc::new(Control::new());
        let (tracked, effects) = channel.track_effects();
        let mut epoch = 1;
        let mut client = tracked.authorized(control.clone(), epoch, false);
        let mut work: Vec<Work> = Vec::new();
        let mut history: Vec<(Receipt, Settlement)> = Vec::new();
        let mut possible = false;
        let mut terminal = None;
        let mut steps = 0;
        for _ in 0..32 {
            steps += 1;
            let choice = random(&mut rng) % 15;
            match choice {
                0..=2 => {
                    let effect = choice != 0;
                    let command = if effect {
                        Command::CreateTab
                    } else {
                        Command::Tree
                    };
                    match client.submit(command, deadline()) {
                        Ok(call) => {
                            assert_eq!(control.snapshot(), (epoch, false));
                            let sent = request(&mut browser).await;
                            assert_eq!(sent["id"], call.receipt.id());
                            if effect {
                                possible = true;
                                hit(&mut counts, "submit_effect");
                            } else {
                                hit(&mut counts, "submit_read");
                            }
                            work.push(Work {
                                receipt: call.receipt.clone(),
                                call: Some(call),
                                effect,
                                epoch,
                            });
                        }
                        Err(Error::Rejected(error)) => {
                            assert!(matches!(
                                error.code,
                                "browser_control_changed" | "browser_busy"
                            ));
                            assert_eq!(error.state, "not_sent");
                            hit(&mut counts, "pre_enqueue_refusal");
                        }
                        Err(error) => {
                            panic!("seed {rng} sequence {sequence}: unexpected submit {error:?}")
                        }
                    }
                }
                3 if !work.is_empty() => {
                    let index = (random(&mut rng) as usize) % work.len();
                    if work[index].call.take().is_some() {
                        hit(&mut counts, "cancel_after_write");
                    }
                }
                4 if !work.is_empty() => {
                    let index = (random(&mut rng) as usize) % work.len();
                    if let Some(mut call) = work[index].call.take() {
                        call.deadline =
                            (tokio::time::Instant::now() - Duration::from_secs(1)).into_std();
                        let error = call.wait().await.unwrap_err();
                        if control.snapshot() == (work[index].epoch, false) {
                            assert!(matches!(error, Error::TimeoutUnknown));
                            hit(&mut counts, "timeout_after_write");
                        } else {
                            assert!(matches!(error, Error::DeliveryRevoked));
                            hit(&mut counts, "timeout_after_revocation");
                        }
                    }
                }
                5 | 6 if !work.is_empty() => {
                    let index = (random(&mut rng) as usize) % work.len();
                    let mut settled = work.swap_remove(index);
                    let success = choice == 5;
                    let expected = if success {
                        Settlement::Success
                    } else {
                        Settlement::ProtocolError(ProtocolError::NoSuchFrame)
                    };
                    let id = settled.receipt.id();
                    if success {
                        send(
                            &mut browser,
                            json!({"type":"success","id":id,"result":{"synthetic":id}}),
                        )
                        .await;
                    } else {
                        send(&mut browser,json!({"type":"error","id":id,"error":"no such frame","message":"synthetic"})).await;
                    }
                    processed(&mut events, &mut browser).await;
                    assert_eq!(settled.receipt.status(), expected);
                    if let Some(call) = settled.call.take() {
                        let result = call.wait().await;
                        if control.snapshot() != (settled.epoch, false) {
                            assert!(matches!(result, Err(Error::DeliveryRevoked)));
                            hit(&mut counts, "settled_delivery_revoked");
                        } else if success {
                            assert_eq!(result.unwrap().value["synthetic"], id);
                        } else {
                            assert!(matches!(
                                result,
                                Err(Error::Protocol(ProtocolError::NoSuchFrame))
                            ));
                        }
                        hit(
                            &mut counts,
                            if success {
                                "response_success"
                            } else {
                                "response_error"
                            },
                        );
                    } else {
                        hit(
                            &mut counts,
                            if success {
                                "late_success"
                            } else {
                                "late_error"
                            },
                        );
                    }
                    // Neither a duplicate nor a future unmatched ID can change
                    // this settlement or resolve another pending command.
                    send(
                        &mut browser,
                        json!({"type":"success","id":id,"result":{"duplicate":true}}),
                    )
                    .await;
                    send(
                        &mut browser,
                        json!({"type":"success","id":MAX_ID-1,"result":{"future":true}}),
                    )
                    .await;
                    processed(&mut events, &mut browser).await;
                    history.push((settled.receipt, expected));
                    hit(&mut counts, "duplicate_and_unknown_reply");
                }
                7 => {
                    control.change(true);
                    hit(&mut counts, "fence");
                    assert!(matches!(
                        client.submit(Command::CreateTab, deadline()),
                        Err(Error::Rejected(Failure {
                            code: "browser_control_changed",
                            ..
                        }))
                    ));
                }
                8 => {
                    if work.iter().all(|w| !w.effect) {
                        epoch = control.change(false);
                        client = tracked.authorized(control.clone(), epoch, false);
                        hit(&mut counts, "new_epoch_after_barrier");
                    } else {
                        hit(&mut counts, "handoff_blocked_by_pending_effect");
                    }
                }
                9 => {
                    event(&mut browser, "observed").await;
                    events.recv().await.unwrap();
                    hit(&mut counts, "critical_event_consumed");
                }
                10 => {
                    let (queue, _receiver) = mpsc::channel(1);
                    queue.try_send("full".into()).unwrap();
                    let blocked = Bidi {
                        outgoing: queue,
                        ..client.clone()
                    };
                    assert!(matches!(
                        blocked.submit(Command::CreateTab, deadline()),
                        Err(Error::Rejected(_))
                    ));
                    hit(&mut counts, "queue_refusal");
                }
                11 => {
                    browser.close(None).await.unwrap();
                    terminal = Some(Fault::Disconnected);
                    hit(&mut counts, "disconnect_after_acceptance");
                }
                12 => {
                    channel.graceful_stop();
                    assert_eq!(channel.snapshot()["pending"], work.len());
                    assert_eq!(request(&mut browser).await["method"], "browser.close");
                    assert!(!channel.connected());
                    browser.close(None).await.unwrap();
                    terminal = Some(Fault::Disconnected);
                    hit(&mut counts, "reserved_close");
                }
                13 => {
                    for _ in 0..=EVENT_COUNT {
                        event(&mut browser, "gap").await;
                    }
                    terminal = Some(Fault::EventGap);
                    hit(&mut counts, "critical_overflow_before_receive");
                }
                14 => {
                    browser.send(Message::Text("[]".into())).await.unwrap();
                    terminal = Some(Fault::Malformed);
                    hit(&mut counts, "malformed_frame");
                }
                _ => {
                    hit(&mut counts, "empty_selection");
                }
            }
            assert_eq!(
                effects.load(Ordering::Acquire),
                possible,
                "sequence {sequence} seed {rng}: effect progress changed"
            );
            for (receipt, expected) in &history {
                assert_eq!(
                    receipt.status(),
                    *expected,
                    "sequence {sequence}: terminal receipt changed"
                );
            }
            if let Some(fault) = terminal {
                assert_eq!(stopped(&channel).await, fault);
                for pending in &work {
                    assert_eq!(pending.receipt.status(), Settlement::Unknown(fault));
                }
                assert_eq!(channel.snapshot()["pending"], 0);
                break;
            }
            let snapshot = channel.snapshot();
            assert_eq!(
                snapshot["pending"],
                work.len(),
                "sequence {sequence} seed {rng}"
            );
            assert_eq!(
                snapshot["effects"],
                work.iter().filter(|w| w.effect).count()
            );
            assert_eq!(snapshot["free_slots"], SLOTS - work.len());
            for pending in &work {
                assert_eq!(pending.receipt.status(), Settlement::Enqueued);
            }
            assert!(
                browser.next().now_or_never().is_none(),
                "unrequested replay in sequence {sequence}"
            );
            hit(&mut counts, "step_invariants");
        }
        steps += 1;
        assert!(steps <= 64);
        max_steps = max_steps.max(steps);
        finish(&channel, events).await;
        if terminal.is_none() {
            for pending in &work {
                assert_eq!(pending.receipt.status(), Settlement::Unknown(Fault::Closed));
            }
        }
        for (receipt, expected) in &history {
            assert_eq!(receipt.status(), *expected);
        }
        assert_eq!(effects.load(Ordering::Acquire), possible);
        hit(&mut counts, "complete_sequences");
    }
    for transition in [
        "submit_effect",
        "submit_read",
        "pre_enqueue_refusal",
        "cancel_after_write",
        "timeout_after_write",
        "timeout_after_revocation",
        "late_success",
        "late_error",
        "settled_delivery_revoked",
        "duplicate_and_unknown_reply",
        "fence",
        "new_epoch_after_barrier",
        "handoff_blocked_by_pending_effect",
        "critical_event_consumed",
        "queue_refusal",
        "disconnect_after_acceptance",
        "reserved_close",
        "critical_overflow_before_receive",
        "malformed_frame",
    ] {
        assert!(
            counts.get(transition).is_some_and(|count| *count > 0),
            "uncovered transition {transition}"
        );
    }
    let report = json!({"implementation":"bidi","sequences":10000,"max_steps":max_steps,"step_limit":64,"seeds":SEEDS.map(|seed|format!("{seed:016x}")),"coverage":counts,"invariants_passed":true,"replay_count":0});
    println!("D2B_SEQUENCES={report}");
    if let Some(directory) = std::env::var_os("RDH_D2B_EVIDENCE") {
        crate::storage::write_private_new(
            &std::path::PathBuf::from(directory).join("wire-sequences.json"),
            &serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
