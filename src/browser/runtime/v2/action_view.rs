//! A bounded, live semantic view accompanying a successful same-tab action.
use super::*;

type ItemKey = (sem::Context, u64, u64, String);
pub(super) type Baseline = HashMap<ItemKey, usize>;

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
) -> Baseline {
    let mut counts = HashMap::new();
    for group in view["frames"].as_array().into_iter().flatten() {
        if let Some(frame) = frame_for(group, ids, frames) {
            for item in group["items"].as_array().into_iter().flatten() {
                *counts.entry(item_key(frame, item)).or_default() += 1;
            }
        }
    }
    counts
}

async fn revisions(
    page: &sem::Page<native::Port>,
    deadline: Instant,
) -> Result<Vec<(sem::Frame, u64)>> {
    let port = page.access(false)?;
    let frames = port.frames(deadline).await?;
    if frames.is_empty() || frames.len() > sem::MAX_FRAMES {
        return Err(sem::types::stale());
    }
    let mut samples = Vec::new();
    for frame in frames {
        let sem::program::Output::ViewRevision(Some(revision)) = port
            .program(&frame, sem::program::Program::ViewRevision, deadline)
            .await?
        else {
            return Err(sem::types::stale());
        };
        samples.push((frame, revision));
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
    let initial = revisions(page, deadline).await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let (view, _) = observe(
        session,
        page,
        port,
        &json!({"max_nodes":100,"max_chars":16000}),
        epoch,
        deadline,
    )
    .await?;
    let final_revisions = revisions(page, deadline).await?;
    if initial != final_revisions || view["coverage"]["changed_during_read"] == true {
        return Err(sem::types::stale());
    }
    let mut overview = view["frame_overview"].clone();
    let mut delta = Value::Null;
    if let Some(mut remaining) = previous {
        let frames = final_revisions
            .into_iter()
            .map(|(f, _)| f)
            .collect::<Vec<_>>();
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
