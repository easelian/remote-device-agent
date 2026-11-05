use super::{
    Result,
    bridge::Bridge,
    browser::{Browser, rss_kib},
    cdp::{Cdp, evaluate},
    fixture::Fixture,
    page,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn check(ok: bool, name: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(name.into()) }
}
fn record(report: &mut Value, name: &str, evidence: Value) {
    report["checks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":name,"status":"passed","evidence":evidence}));
}

async fn settled(fixture: &Fixture, saves: usize, frame_clicks: usize) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            {
                let records = fixture.records.lock().unwrap();
                if records.saves == saves && records.frame_clicks == frame_clicks {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "site_counter_mismatch")?;
    Ok(())
}

fn percentile(mut samples: Vec<f64>) -> Value {
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    json!({"samples":n,"p50_ms":samples[(n-1)/2],"p95_ms":samples[(n*95).div_ceil(100)-1],"max_ms":samples[n-1],"raw_ms":samples})
}

async fn screenshot(cdp: &Cdp, session: &str, path: &Path) -> Result<usize> {
    let result = cdp
        .call(
            Some(session),
            "Page.captureScreenshot",
            json!({"format":"png"}),
        )
        .await?;
    let bytes = STANDARD.decode(result["data"].as_str().ok_or("missing_screenshot")?)?;
    check(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "invalid_png")?;
    let size = bytes.len();
    fs::write(path, bytes)?;
    Ok(size)
}

async fn scenarios(executable: &Path, output: &Path, report: &mut Value) -> Result<()> {
    let fixture = Fixture::start().await?;
    let mut browser = Browser::launch(executable, output, "main").await?;
    report["browser_version"] = browser.version.clone();
    report["startup_ms"] = json!(browser.startup_ms);
    let cdp = browser.cdp.clone();
    let session = browser.session.clone();
    let mut events = cdp.subscribe();
    cdp.call(
        Some(&session),
        "Emulation.setDeviceMetricsOverride",
        json!({"width":1280,"height":720,"deviceScaleFactor":1,"mobile":false}),
    )
    .await?;
    page::navigate(&cdp, &session, &fixture.url()).await?;
    let frame_session = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await?;
            if event["method"] == "Target.attachedToTarget"
                && event["params"]["targetInfo"]["type"] == "iframe"
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    event["params"]["sessionId"]
                        .as_str()
                        .ok_or("no_iframe_session")?
                        .to_owned(),
                );
            }
        }
    })
    .await??;
    let initial = page::observe(&cdp, &session).await?;
    let processes = browser.process_evidence().await?;
    report["renderer_process_evidence"] = processes.clone();
    let renderers: Vec<_> = processes["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["renderer"] == true)
        .collect();
    check(
        !renderers.is_empty()
            && renderers
                .iter()
                .all(|p| p["seccomp"] == "2" && p["no_new_privs"] == "1"),
        "renderer_sandbox_evidence_missing",
    )?;
    record(report, "renderer_sandbox", processes);
    fs::write(
        output.join("observation.json"),
        serde_json::to_vec_pretty(&initial)?,
    )?;
    let contact = page::find(&cdp, &session, "Contact", "textbox").await?;
    contact.fill(&cdp, "测试客户🙂 e\u{301}").await?;
    page::find(&cdp, &session, "Save record", "button")
        .await?
        .click(&cdp)
        .await?;
    settled(&fixture, 1, 0).await?;
    check(
        fixture.records.lock().unwrap().value == "测试客户🙂 e\u{301}",
        "unicode_form_value",
    )?;
    record(
        report,
        "semantic_form_and_unicode",
        json!({"independent_site_saves":1,"value_verified":true,"input_method":"Input.insertText"}),
    );

    let frame_observation = page::observe(&cdp, &frame_session).await?;
    check(
        !initial["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Frame action"),
        "unexpected_iframe_in_main_ax_tree",
    )?;
    page::find(&cdp, &frame_session, "Frame action", "button")
        .await?
        .click(&cdp)
        .await?;
    settled(&fixture, 1, 1).await?;
    record(
        report,
        "out_of_process_iframe",
        json!({"distinct_session":frame_session!=session,
        "main_tree_omits_child_content":true,"child_nodes":frame_observation["nodes"].as_array().unwrap().len(),"independent_site_clicks":1}),
    );
    check(
        page::find(&cdp, &session, "Shadow action", "button")
            .await
            .is_ok(),
        "open_shadow_missing",
    )?;
    record(
        report,
        "open_shadow_semantics",
        json!({"button_observed":true}),
    );

    let stale = page::find(&cdp, &session, "Replaceable", "button").await?;
    evaluate(&cdp, &session, "replaceTarget(); true").await?;
    check(
        stale.check(&cdp).await.is_err(),
        "detached_reference_accepted",
    )?;
    evaluate(
        &cdp,
        &session,
        "document.getElementById('overlay').style.display='block';true",
    )
    .await?;
    check(
        contact.check(&cdp).await.is_err(),
        "occluded_target_accepted",
    )?;
    evaluate(
        &cdp,
        &session,
        "document.getElementById('overlay').style.display='none';true",
    )
    .await?;
    record(
        report,
        "stale_and_occlusion",
        json!({"detached_rejected":true,"covered_rejected":true}),
    );
    for url in [
        "file:///etc/passwd",
        "javascript:document.title='bad'",
        "chrome://settings",
    ] {
        check(
            page::navigate(&cdp, &session, url).await.is_err(),
            "special_navigation_accepted",
        )?;
    }
    record(
        report,
        "direct_navigation_scheme",
        json!({"rejected":3,"redirect_and_link_interception":"not_implemented"}),
    );

    // Renderer busy-loop is deliberately fixed test code, never exposed as a user tool.
    contact.fill(&cdp, "").await?;
    let start = Instant::now();
    let busy=cdp.call_for(Some(&session),"Runtime.evaluate",json!({"expression":"{const end=Date.now()+800;while(Date.now()<end){};true}","returnByValue":true}),Duration::from_millis(25)).await;
    check(busy.is_err(), "busy_renderer_did_not_timeout")?;
    let input = cdp
        .call_for(
            Some(&session),
            "Input.insertText",
            json!({"text":"LATE"}),
            Duration::from_millis(25),
        )
        .await;
    check(input.is_err(), "delayed_input_did_not_timeout")?;
    let pending = cdp.pending();
    check(pending >= 2, "timeout_forgot_pending_commands")?;
    check(
        Bridge::start(cdp.clone(), &session).await.is_err(),
        "handoff_accepted_pending_input",
    )?;
    cdp.call(None, "Browser.getVersion", json!({})).await?;
    let pending_after_browser_reply = cdp.pending();
    check(
        pending_after_browser_reply >= 2,
        "browser_barrier_did_not_exercise_delayed_input",
    )?;
    cdp.wait_idle().await?;
    check(
        evaluate(&cdp, &session, "contact.value === 'LATE'").await? == true,
        "late_input_not_observed",
    )?;
    record(
        report,
        "timeout_and_handoff",
        json!({"pending_at_timeout":pending,"pending_after_browser_level_reply":pending_after_browser_reply,
        "late_replies":cdp.late_replies(),"late_effect_observed":true,"handoff_rejected_while_pending":true,"elapsed_ms":start.elapsed().as_secs_f64()*1000.}),
    );

    // Keep timed-out commands admitted until their actual replies; reject slot 33.
    let _ = cdp
        .call_for(
            Some(&session),
            "Runtime.evaluate",
            json!({"expression":"{const end=Date.now()+1200;while(Date.now()<end){};true}"}),
            Duration::from_millis(2),
        )
        .await;
    for _ in 0..31 {
        let _ = cdp
            .call_for(
                Some(&session),
                "Runtime.evaluate",
                json!({"expression":"true"}),
                Duration::from_millis(2),
            )
            .await;
    }
    check(cdp.pending() == 32, "pending_capacity_not_exercised")?;
    let overflow = cdp.call(None, "Browser.getVersion", json!({})).await;
    check(
        overflow.is_err_and(|e| e.to_string() == "cdp_busy"),
        "pending_overflow_not_rejected",
    )?;
    cdp.wait_idle().await?;
    record(
        report,
        "bounded_outstanding_commands",
        json!({"held_after_timeout":32,"overflow_rejected":true,"after_replies":cdp.pending()}),
    );

    let password = page::find(&cdp, &session, "Password", "textbox").await?;
    let point = password.check(&cdp).await?;
    let bridge = Bridge::start(cdp.clone(), &session).await?;
    check(
        bridge.ai_observe().await.is_err(),
        "human_mode_observation_disclosed",
    )?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client.post(format!("{}/frame", bridge.url)).send().await?;
    check(response.status() == 403, "missing_key_accepted")?;
    let frame: Value = client
        .post(format!("{}/frame", bridge.url))
        .header("origin", &bridge.url)
        .header("x-p0-key", &bridge.key)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let click = json!({"type":"click","x":point["x"],"y":point["y"],"epoch":bridge.epoch,"frame":frame["frame"],"sequence":1});
    client
        .post(format!("{}/input", bridge.url))
        .header("origin", &bridge.url)
        .header("x-p0-key", &bridge.key)
        .json(&click)
        .send()
        .await?
        .error_for_status()?;
    let duplicate = client
        .post(format!("{}/input", bridge.url))
        .header("origin", &bridge.url)
        .header("x-p0-key", &bridge.key)
        .json(&click)
        .send()
        .await?;
    check(duplicate.status() == 409, "duplicate_input_accepted")?;
    // Model the user's separate browser, outside the controlled CDP connection.
    let mut viewer = Browser::launch(executable, output, "viewer").await?;
    let viewer_cdp = viewer.cdp.clone();
    let viewer_session = viewer.session.clone();
    page::navigate(
        &viewer_cdp,
        &viewer_session,
        &format!("{}/#{}", bridge.url, bridge.key),
    )
    .await?;
    let image_loaded = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if evaluate(
                &viewer_cdp,
                &viewer_session,
                "document.getElementById('screen').naturalWidth > 0",
            )
            .await?
                == true
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    if image_loaded.is_err() {
        let _ = screenshot(
            &viewer_cdp,
            &viewer_session,
            &output.join("viewer-failure.png"),
        )
        .await;
        report["viewer_status"] = evaluate(
            &viewer_cdp,
            &viewer_session,
            "document.getElementById('status').textContent",
        )
        .await?;
        return Err("viewer_image_timeout".into());
    }
    image_loaded??;
    // The preceding HTTP probe used sequence 1. Set up the isolated UI's next sequence.
    evaluate(&viewer_cdp,&viewer_session,"sequence=1;document.getElementById('text').value='P0-only-密码-7391';document.getElementById('send').click();true").await?;
    let secret_delivered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if evaluate(
                &viewer_cdp,
                &viewer_session,
                "!busy && document.getElementById('text').value === ''",
            )
            .await?
                == true
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    if secret_delivered.is_err() {
        report["viewer_status"] = evaluate(
            &viewer_cdp,
            &viewer_session,
            "document.getElementById('status').textContent",
        )
        .await?;
        return Err("viewer_input_timeout".into());
    }
    secret_delivered??;
    check(
        evaluate(&cdp, &session, "password.value === 'P0-only-密码-7391'").await? == true,
        "viewer_secret_value_mismatch",
    )?;
    // Do not save screenshots until the synthetic password has been cleared from both pages.
    evaluate(&cdp, &session, "password.value='';true").await?;
    evaluate(
        &viewer_cdp,
        &viewer_session,
        "document.getElementById('text').value='';refresh().then(()=>true)",
    )
    .await?;
    cdp.wait_idle().await?;
    let viewer_bytes = screenshot(
        &viewer_cdp,
        &viewer_session,
        &output.join("handoff-viewer.png"),
    )
    .await?;
    bridge.return_to_ai().await?;
    let after = bridge.ai_observe().await?;
    check(
        !serde_json::to_string(&after)?.contains("P0-only-密码-7391"),
        "secret_in_observation",
    )?;
    let stale_input = client
        .post(format!("{}/input", bridge.url))
        .header("origin", &bridge.url)
        .header("x-p0-key", &bridge.key)
        .json(&click)
        .send()
        .await?;
    check(stale_input.status() == 409, "old_controller_accepted")?;
    let viewer_cleanup = viewer.close().await?;
    record(
        report,
        "local_handoff_viewer",
        json!({"real_viewer_image_loaded":true,"unicode_secret_inserted_via_ui":true,
        "duplicate_rejected":true,"old_epoch_rejected":true,"ai_observation_blocked_in_human_mode":true,"secret_not_in_resumed_observation":true,"viewer_png_bytes":viewer_bytes,
        "viewer_cleanup":viewer_cleanup,"scope":"loopback prototype, no Hub auth, no human usability claim"}),
    );
    drop(bridge);
    let png_bytes = screenshot(&cdp, &session, &output.join("fixture.png")).await?;
    record(
        report,
        "screenshot",
        json!({"png_bytes":png_bytes,"viewport":[1280,720]}),
    );

    // Declared measurement protocol: 5 warmups followed by 100 serial samples, same local fixture.
    let mut observations = Vec::new();
    let mut actions = Vec::new();
    let rss_before = rss_kib();
    for i in 0..105 {
        let start = Instant::now();
        page::observe(&cdp, &session).await?;
        let observation_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        contact.fill(&cdp, "baseline").await?;
        let action_ms = start.elapsed().as_secs_f64() * 1000.;
        if i >= 5 {
            observations.push(observation_ms);
            actions.push(action_ms);
        }
    }
    report["measurements"] = json!({"warmups":5,"observation":percentile(observations),"fill_including_checks":percentile(actions),
        "harness_rss_before_kib":rss_before,"harness_rss_after_kib":rss_kib(),"scope":"one small local fixture; no Hub, model or network emulation"});
    let old = contact.clone();
    page::navigate(&cdp, &session, &fixture.url()).await?;
    check(old.check(&cdp).await.is_err(), "old_document_accepted")?;
    record(
        report,
        "navigation_invalidates_reference",
        json!({"old_reference_rejected":true}),
    );
    record(report, "normal_cleanup", browser.close().await?);

    // A full AX tree can exceed transport budget before any output-node truncation is useful.
    let mut large = Browser::launch(executable, output, "large").await?;
    page::navigate(&large.cdp, &large.session, &fixture.url()).await?;
    evaluate(&large.cdp,&large.session,"document.body.innerHTML='<main>'+Array.from({length:30000},(_,i)=>'<p>Row '+i+'</p>').join('')+'</main>';true").await?;
    let oversized = page::observe(&large.cdp, &large.session).await;
    check(oversized.is_err(), "oversized_ax_response_not_rejected")?;
    let connected = large.cdp.connected();
    let close_reason = large.cdp.close_reason();
    check(
        close_reason == Some("message_capacity"),
        "oversized_failure_cause_unconfirmed",
    )?;
    let cleanup = large.close().await?;
    record(
        report,
        "oversized_ax_tree",
        json!({"paragraphs":30000,"observation_failed_with_limit":true,"connection_survived":connected,"close_reason":close_reason,"cleanup":cleanup}),
    );

    // Crash one owned browser while another isolated browser remains alive.
    let mut survivor = Browser::launch(executable, output, "survivor").await?;
    let mut crashed = Browser::launch(executable, output, "crashed").await?;
    let crashed_cdp = crashed.cdp.clone();
    crashed.child.kill()?;
    crashed.child.wait()?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    check(
        crashed_cdp
            .call(None, "Browser.getVersion", json!({}))
            .await
            .is_err(),
        "dead_browser_accepted_command",
    )?;
    check(
        Bridge::start(crashed_cdp.clone(), &crashed.session)
            .await
            .is_err(),
        "disconnected_handoff_accepted",
    )?;
    let crash_cleanup = crashed.close().await?;
    survivor
        .cdp
        .call(None, "Browser.getVersion", json!({}))
        .await?;
    record(
        report,
        "browser_crash_cleanup_and_isolation",
        json!({"cleanup":crash_cleanup,"other_browser_alive":true,"disconnected_handoff_rejected":true}),
    );
    record(report, "second_normal_cleanup", survivor.close().await?);
    Ok(())
}

pub async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err(
            "Usage: cargo run --example browser_p0 -- /absolute/browser /new/output-directory"
                .into(),
        );
    }
    let executable = PathBuf::from(&args[0]).canonicalize()?;
    let output = PathBuf::from(&args[1]);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::DirBuilder::new().mode(0o700).create(&output)?;
    let mut sources = serde_json::Map::new();
    for entry in fs::read_dir("examples/browser_p0")? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "rs") {
            sources.insert(
                path.to_string_lossy().into(),
                json!(format!("{:x}", Sha256::digest(fs::read(path)?))),
            );
        }
    }
    let revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()?;
    let mut report = json!({"status":"running","source_revision":String::from_utf8(revision.stdout)?.trim(),"source_files_sha256":sources,
        "build_profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "browser_path":executable,"environment":{"os_release":fs::read_to_string("/etc/os-release")?,"arch":std::env::consts::ARCH},
        "checks":[],"limitations":["Linux only","small synthetic pages","no Hub/MCP integration","no production auth or receipt integration","no external sites","no cross-platform acceptance"]});
    let result = scenarios(&executable, &output, &mut report).await;
    match &result {
        Ok(()) => report["status"] = "passed".into(),
        Err(e) => {
            report["status"] = "failed".into();
            report["failure"] = e.to_string().into();
        }
    }
    // Scan emitted text evidence for our synthetic secret. This is not a general privacy proof.
    let mut leaked = false;
    for entry in fs::read_dir(&output)? {
        let path = entry?.path();
        if path.is_file()
            && let Ok(bytes) = fs::read(&path)
            && bytes
                .windows("P0-only-密码-7391".len())
                .any(|w| w == "P0-only-密码-7391".as_bytes())
        {
            leaked = true;
        }
    }
    report["synthetic_secret_found_in_text_artifacts"] = json!(leaked);
    if leaked {
        report["status"] = "failed".into();
    }
    fs::write(
        output.join("results.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "{} checks; status={}; report={}",
        report["checks"].as_array().unwrap().len(),
        report["status"],
        output.join("results.json").display()
    );
    result?;
    check(!leaked, "synthetic_secret_leaked")
}
