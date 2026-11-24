//! A bounded, live view of what changed on the page, accompanying a successful
//! same-tab action.
//!
//! Each document records which of its parts were altered since they were last
//! read. The view reads only those parts, in the compact layout, and adds fresh
//! references for them. References to the rest of the page stay as they were.
use super::*;

/// How long the page is left alone after an input before it is looked at.
const PAUSE: Duration = Duration::from_millis(20);
/// How long the answers to what an input asked the site for are waited for.
const ASKED: Duration = Duration::from_secs(2);
/// How long a movement on the page is waited for before the page is looked at as it is.
const SETTLE: Duration = Duration::from_millis(700);

async fn revisions(
    port: &native::Port,
    frames: &[sem::Frame],
    deadline: Instant,
) -> Result<HashMap<sem::Context, u64>> {
    let mut samples = HashMap::new();
    for frame in frames {
        let sem::program::Output::ViewRevision(Some(revision)) = port
            .program(
                frame,
                sem::program::Program::ViewRevision(frame.marker.clone()),
                deadline,
            )
            .await?
        else {
            return Err(sem::types::stale());
        };
        samples.insert(frame.context, revision);
    }
    Ok(samples)
}

async fn read(
    session: &mut Session,
    page: &sem::Page<native::Port>,
    port: &native::Port,
    epoch: u64,
    deadline: Instant,
    asked: &Asked,
) -> Result<Value> {
    let reader = page.access(false)?;
    let frames = reader.frames(deadline).await?;
    // A document that was not there at the last observation has recorded no
    // change of its own, so its content would be missed. An address changed
    // by the page's own history calls leaves the document in place.
    let known = &session.observed_frames;
    if frames.len() != known.len()
        || frames.iter().any(|frame| {
            !known.iter().any(|seen| {
                (seen.context, seen.parent, seen.document, seen.realm)
                    == (frame.context, frame.parent, frame.document, frame.realm)
            })
        })
    {
        return Err(sem::types::stale());
    }
    // Sample both sides of the read. A page with a clock or a live feed never
    // stops changing; that is reported, and the references of the view are
    // checked again before any input is sent.
    let initial = revisions(&reader, &frames, deadline).await?;
    // What a handler does at once is done before this point. A page that goes
    // on changing after it is given as long again, so that a redraw under way
    // is read whole, or is seen not to have ended.
    tokio::time::sleep(PAUSE).await;
    // A click that adds to a basket has done nothing the page shows until
    // the site has answered. Requests a script made since the input are
    // waited for, up to two seconds, and the script that takes the answer
    // is given a moment to put it on the page.
    if let Some((mark, sink)) = asked {
        let patience = deadline.min(Instant::now() + ASKED);
        let mut waited = false;
        while Instant::now() < patience
            && sink
                .journal_read(|log| log.asking(*mark))
                .is_some_and(|open| open > 0)
        {
            waited = true;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if waited {
            tokio::time::sleep(PAUSE * 3).await;
        }
    }
    if revisions(&reader, &frames, deadline).await? != initial {
        tokio::time::sleep(PAUSE).await;
    }
    // What is on its way in or out is waited for: a dialog that comes after
    // its backdrop has faded in, a drawer that slides. What follows one such
    // movement is given a moment to begin.
    let settled = deadline.min(Instant::now() + SETTLE);
    let mut moved = false;
    loop {
        let mut left = 0;
        for frame in &frames {
            if let sem::program::Output::Revision(rest) = reader
                .program(frame, sem::program::Program::Settling, deadline)
                .await?
            {
                left = left.max(rest);
            }
        }
        let now = Instant::now();
        if now >= settled || left == 0 && !moved {
            break;
        }
        moved = left > 0;
        let pause = if moved {
            Duration::from_millis(left) + PAUSE
        } else {
            PAUSE
        };
        tokio::time::sleep(pause.min(settled - now)).await;
    }
    let (mut view, _) = observe_frames(
        session,
        page,
        port,
        &json!({"max_nodes":100,"max_chars":16000,"format":"compact","changed":true}),
        epoch,
        deadline,
        sem::ReadFrames {
            tree: &frames,
            changed: None,
        },
    )
    .await?;
    let omitted = |name: &str| {
        view["coverage"]["omitted"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item == name))
    };
    if omitted("changed_region_limit") {
        return Err(sem::types::stale());
    }
    let settled = view["coverage"]["changed_during_read"] != true
        && revisions(&reader, &frames, deadline).await? == initial;
    let removed = omitted("changed_content_removed");
    let text = view["text"].take();
    if text.as_str().is_none_or(str::is_empty) && !removed {
        return Ok(json!({"state":"continue_observing","reason":"no_visible_change"}));
    }
    // The three flags are written only when they depart from the ordinary case:
    // nothing disappeared, the page had settled, and every changed part fitted.
    let mut sampled = json!({"state":"sampled","content_trust":"untrusted_page",
        "text":text});
    if removed {
        sampled["removed"] = true.into();
    }
    if !settled {
        sampled["settled"] = false.into();
    }
    if view["coverage"]["complete"] != true {
        sampled["truncated"] = true.into();
    }
    Ok(sampled)
}

/// Where the page's requests stood before the input, and the journal that
/// tells which of those made since are still without an answer.
pub(super) type Asked = Option<(u64, sem::task_events::Sink)>;

pub(super) async fn sample(
    session: &mut Session,
    page: &sem::Page<native::Port>,
    port: &native::Port,
    epoch: u64,
    deadline: Instant,
    asked: Asked,
) -> Value {
    // The write is already acknowledged. A slow optional read must not exhaust
    // its delivery budget or turn it into a write failure.
    let now = Instant::now();
    let budget = deadline
        .saturating_duration_since(now)
        .saturating_sub(Duration::from_millis(200))
        // Waiting for the site's answers takes its own time on top.
        .min(
            Duration::from_millis(900)
                + if asked.is_some() {
                    ASKED
                } else {
                    Duration::ZERO
                },
        );
    let until = now + budget;
    if budget > Duration::from_millis(40)
        && let Ok(Ok(view)) = tokio::time::timeout_at(
            until.into(),
            read(session, page, port, epoch, until, &asked),
        )
        .await
    {
        return view;
    }
    // A read that failed part-way may have published only some regions.
    session.clear_references();
    json!({"state":"continue_observing","reason":"view_unavailable_or_changing"})
}
