//! A bounded, live view of what changed on the page, accompanying a successful
//! same-tab action.
//!
//! Each document records which of its parts were altered since they were last
//! read. The view reads only those parts, in the compact layout, and adds fresh
//! references for them. References to the rest of the page stay as they were.
use super::*;

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
    tokio::time::sleep(Duration::from_millis(40)).await;
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
    let groups = view["frames"].take();
    if groups.as_array().is_none_or(Vec::is_empty) && !removed {
        return Ok(json!({"state":"continue_observing","reason":"no_visible_change"}));
    }
    Ok(
        json!({"state":"sampled","content_trust":"untrusted_page","scope":"changed_regions",
        "format":"compact_v1","frames":groups,"records":view["records"].take(),
        "removed":removed,"settled":settled,"truncated":view["coverage"]["complete"] != true}),
    )
}

pub(super) async fn sample(
    session: &mut Session,
    page: &sem::Page<native::Port>,
    port: &native::Port,
    epoch: u64,
    deadline: Instant,
) -> Value {
    // The write is already acknowledged. A slow optional read must not exhaust
    // its delivery budget or turn it into a write failure.
    let now = Instant::now();
    let budget = deadline
        .saturating_duration_since(now)
        .saturating_sub(Duration::from_millis(200))
        .min(Duration::from_millis(900));
    let until = now + budget;
    if budget > Duration::from_millis(40)
        && let Ok(Ok(view)) =
            tokio::time::timeout_at(until.into(), read(session, page, port, epoch, until)).await
    {
        return view;
    }
    // A read that failed part-way may have published only some regions.
    session.clear_references();
    json!({"state":"continue_observing","reason":"view_unavailable_or_changing"})
}
