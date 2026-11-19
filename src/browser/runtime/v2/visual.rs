//! One-use image coordinates for the AI that owns this browser session.
use super::*;
use sha2::{Digest, Sha256};

const FRAME_LIFETIME: Duration = Duration::from_secs(30);

pub(super) struct Frame {
    id: String,
    tab: String,
    epoch: u64,
    documents: Vec<sem::Frame>,
    captured: Instant,
    revision: u64,
    viewport: (f64, f64),
    scroll: (f64, f64),
    digest: [u8; 32],
}

fn digest(image: &Value) -> Result<[u8; 32]> {
    let data = image["data"].as_str().ok_or_else(sem::types::decode)?;
    Ok(Sha256::digest(data.as_bytes()).into())
}

fn stale() -> Failure {
    Failure::new("browser_stale_frame")
}

async fn revision(port: &native::Port, root: &sem::Frame, deadline: Instant) -> Result<u64> {
    match port
        .program(root, sem::program::Program::VisualRevision, deadline)
        .await?
    {
        sem::program::Output::Revision(value) => Ok(value),
        _ => Err(sem::types::decode()),
    }
}

#[derive(Clone, Copy)]
enum Gesture<T> {
    Click,
    Hover,
    Drag { to_x: T, to_y: T },
}

impl Frame {
    pub(super) async fn capture(
        page: &sem::Page<native::Port>,
        tab: &str,
        epoch: u64,
        deadline: Instant,
    ) -> Result<(Value, Option<Self>)> {
        let port = page.access(false)?;
        let (before, _) = port.metadata(deadline).await?;
        let revision_before = revision(&port, &before, deadline).await?;
        let (mut image, documents, root, meta) = super::capture(&port, deadline).await?;
        image["tab_id"] = tab.into();
        // A changing view can still be read, but cannot authorize a later input.
        if root != before || revision(&port, &root, deadline).await? != revision_before {
            return Ok((image, None));
        }
        let id = format!("vfrm_{}", token()?);
        let frame = Self {
            id: id.clone(),
            tab: tab.to_owned(),
            epoch,
            documents,
            captured: Instant::now(),
            revision: revision_before,
            viewport: (meta.width, meta.height),
            scroll: (meta.scroll_x, meta.scroll_y),
            digest: digest(&image)?,
        };
        image["frame_id"] = id.into();
        Ok((image, Some(frame)))
    }

    pub(super) async fn act(
        self,
        page: &sem::Page<native::Port>,
        tab: &str,
        epoch: u64,
        action: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        if self.id != action["frame_id"]
            || self.tab != tab
            || self.epoch != epoch
            || self.captured.elapsed() >= FRAME_LIFETIME
        {
            return Err(stale());
        }
        let coordinate = |key: &str| {
            action[key]
                .as_u64()
                .ok_or_else(|| Failure::new("browser_invalid_params"))
        };
        let (x, y) = (coordinate("x")?, coordinate("y")?);
        let gesture = match action["kind"].as_str() {
            Some("visual_click") => Gesture::Click,
            Some("visual_hover") => Gesture::Hover,
            Some("visual_drag") => Gesture::Drag {
                to_x: coordinate("to_x")?,
                to_y: coordinate("to_y")?,
            },
            _ => return Err(Failure::new("browser_invalid_params")),
        };
        page.command(false, false, deadline, move |port| async move {
            let (image, documents, root, meta) = super::capture(&port, deadline).await?;
            if !same_frames(&documents, &self.documents)
                || self.viewport != (meta.width, meta.height)
                || self.scroll != (meta.scroll_x, meta.scroll_y)
                || self.digest != digest(&image)?
                || revision(&port, &root, deadline).await? != self.revision
            {
                return Err(stale());
            }
            let width = image["width"].as_u64().ok_or_else(sem::types::decode)?;
            let height = image["height"].as_u64().ok_or_else(sem::types::decode)?;
            let viewport_point = |x: u64, y: u64| {
                if x >= width || y >= height {
                    return Err(Failure::new("browser_invalid_params"));
                }
                Ok((
                    x as f64 * meta.width / width as f64,
                    y as f64 * meta.height / height as f64,
                ))
            };
            let (x, y) = viewport_point(x, y)?;
            if !port.visual_target_allowed(&root, x, y, deadline).await? {
                return Err(Failure::new("browser_not_actionable"));
            }
            let gesture = match gesture {
                Gesture::Click => Gesture::Click,
                Gesture::Hover => Gesture::Hover,
                Gesture::Drag { to_x, to_y } => {
                    let (to_x, to_y) = viewport_point(to_x, to_y)?;
                    let path = sem::program::Program::VisualPath { x, y, to_x, to_y };
                    if (x, y) == (to_x, to_y)
                        || !matches!(
                            port.program(&root, path, deadline).await?,
                            sem::program::Output::Applied(Some(true))
                        )
                    {
                        return Err(Failure::new("browser_not_actionable"));
                    }
                    Gesture::Drag { to_x, to_y }
                }
            };
            if self.captured.elapsed() >= FRAME_LIFETIME
                || port.refresh(&root, deadline).await? != root
                || revision(&port, &root, deadline).await? != self.revision
            {
                return Err(stale());
            }
            let path = match gesture {
                Gesture::Click => {
                    port.visual_click(x, y, deadline).await?;
                    "visual_pointer"
                }
                Gesture::Hover => {
                    port.visual_hover(x, y, deadline).await?;
                    "visual_hover"
                }
                Gesture::Drag { to_x, to_y } => {
                    port.visual_drag(x, y, to_x, to_y, deadline).await?;
                    "visual_drag"
                }
            };
            Ok(json!({"tab_id":self.tab, "changed":true, "path":path,
                "input_acknowledged":true, "business_result_verified":false}))
        })
        .await
    }
}
