//! Viewport-only images with a fixed pixel and encoded-byte budget.
use super::{
    Failure, Result,
    engine::{Context, Engine},
};
use serde_json::{Value, json};
use std::time::Instant;

pub const MAX_ENCODED: usize = 512 * 1024;

pub async fn capture(engine: &Engine, session: &Context, deadline: Instant) -> Result<Value> {
    let viewport = engine.visual_viewport(session, deadline).await?;
    let (width, height) = (viewport.width, viewport.height);
    if !width.is_finite()
        || !height.is_finite()
        || width < 1.
        || height < 1.
        || width > 1920.
        || height > 1920.
    {
        return Err(Failure::new("browser_image_too_large"));
    }
    let data = engine
        .capture_viewport(session, &viewport, deadline)
        .await?;
    if data.is_empty() || data.len() > MAX_ENCODED {
        return Err(Failure::new("browser_image_too_large"));
    }
    Ok(
        json!({"mime_type":"image/jpeg","data":data,"width":width.round() as u64,
        "height":height.round() as u64,"css_pixel_scale":1,"scope":"viewport"}),
    )
}
