//! A bounded, live semantic view accompanying a successful same-tab action.
use super::*;

type ItemKey = (sem::Context, u64, u64, String);
pub(super) struct Baseline {
    items: HashMap<ItemKey, usize>,
    frames: Vec<sem::Frame>,
    revisions: HashMap<sem::Context, u64>,
}

fn item_key(frame: &sem::Frame, item: &Value) -> ItemKey {
    let mut content = item.clone();
    if let Some(fields) = content.as_object_mut() {
        fields.remove("ref");
    }
    (
        frame.context,
        frame.document,
        frame.realm,
        content.to_string(),
    )
}

fn frame_for<'a>(
    group: &Value,
    ids: &HashMap<String, sem::Context>,
    frames: &'a [sem::Frame],
) -> Option<&'a sem::Frame> {
    let context = ids.get(group["frame_id"].as_str()?)?;
    frames.iter().find(|frame| &frame.context == context)
}

pub(super) fn baseline(
    view: &Value,
    ids: &HashMap<String, sem::Context>,
    frames: &[sem::Frame],
    revisions: &HashMap<sem::Context, u64>,
    complete: bool,
) -> Baseline {
    let mut counts = HashMap::new();
    for group in view["frames"].as_array().into_iter().flatten() {
        if let Some(frame) = frame_for(group, ids, frames) {
            for item in group["items"].as_array().into_iter().flatten() {
                *counts.entry(item_key(frame, item)).or_default() += 1;
            }
        }
    }
    Baseline {
        items: counts,
        frames: frames.to_vec(),
        revisions: if complete {
            revisions.clone()
        } else {
            HashMap::new()
        },
    }
}

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
    previous: Option<Baseline>,
    epoch: u64,
    deadline: Instant,
) -> Result<Value> {
    // Sample both sides of a normal observation. A quiet interval is evidence about
    // this sample only; a future response or timer can still change the site.
    let reader = page.access(false)?;
    let frames = reader.frames(deadline).await?;
    let initial = revisions(&reader, &frames, deadline).await?;
    let changed = if frames.len() > 1 || previous.as_ref().is_some_and(|b| b.frames.len() > 1) {
        let baseline = previous.as_ref().ok_or_else(sem::types::stale)?;
        if !same_frames(&frames, &baseline.frames) || baseline.revisions.len() != frames.len() {
            return Err(sem::types::stale());
        }
        let changed: Vec<_> = frames
            .iter()
            .filter(|f| baseline.revisions.get(&f.context) != initial.get(&f.context))
            .map(|f| f.context)
            .collect();
        if changed.is_empty() {
            return Ok(json!({"state":"continue_observing","reason":"no_visible_change"}));
        }
        Some(changed)
    } else {
        None
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    let (view, _) = observe_frames(
        session,
        page,
        port,
        &json!({"max_nodes":100,"max_chars":16000}),
        epoch,
        deadline,
        sem::ReadFrames {
            tree: &frames,
            changed: changed.as_deref(),
        },
    )
    .await?;
    if view["coverage"]["changed_during_read"] == true
        || revisions(&reader, &frames, deadline).await? != initial
    {
        return Err(sem::types::stale());
    }
    let mut overview = view["frame_overview"].clone();
    if changed.is_some() {
        // A partial view never inherits controls or record labels from skipped frames.
        // Fall back if the fresh scan cannot establish a returned control's owner.
        if view["coverage"]["complete"] != true
            || overview["frames"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|g| g["items"].as_array().into_iter().flatten())
                .any(|i| i.get("ref").is_some() && i.get("record").is_none())
        {
            return Err(sem::types::stale());
        }
        overview["scope"] = "changed_frames".into();
    }
    let mut delta = Value::Null;
    if let Some(baseline) = previous {
        let mut remaining = baseline.items;
        if let Some(changed) = &changed {
            let next = session
                .overview_baseline
                .as_mut()
                .ok_or_else(sem::types::stale)?;
            remaining.retain(|key, count| {
                if changed.contains(&key.0) {
                    true
                } else {
                    next.items.insert(key.clone(), *count);
                    false
                }
            });
            next.revisions = initial;
        }
        let mut added = 0;
        for group in overview["frames"].as_array_mut().into_iter().flatten() {
            let frame = frame_for(group, &session.frames, &frames).ok_or_else(sem::types::stale)?;
            for item in group["items"].as_array_mut().into_iter().flatten() {
                let count = remaining.entry(item_key(frame, item)).or_default();
                if *count == 0 {
                    item["appeared"] = true.into();
                    added += 1;
                } else {
                    *count -= 1;
                }
            }
        }
        let removed = remaining.values().sum::<usize>();
        if added == 0 && removed == 0 {
            return Ok(json!({"state":"continue_observing","reason":"no_visible_change"}));
        }
        delta = json!({"appeared":added,"disappeared":removed});
    }
    Ok(
        json!({"state":"sampled","content_trust":"untrusted_page","changes":delta,"overview":overview}),
    )
}

pub(super) async fn sample(
    session: &mut Session,
    page: &sem::Page<native::Port>,
    port: &native::Port,
    previous: Option<Baseline>,
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
        && let Ok(Ok(view)) = tokio::time::timeout_at(
            until.into(),
            read(session, page, port, previous, epoch, until),
        )
        .await
    {
        if view["state"] != "sampled" {
            session.clear_references();
        }
        return view;
    }
    session.clear_references();
    json!({"state":"continue_observing","reason":"view_unavailable_or_changing"})
}
