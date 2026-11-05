//! Viewport-only images with a fixed pixel and encoded-byte budget.
use super::{Failure, Result, cdp::Cdp};
use serde_json::{Value, json};
use std::time::Instant;

pub const MAX_ENCODED: usize = 512 * 1024;

pub async fn capture(cdp: &Cdp, session: &str, deadline: Instant) -> Result<Value> {
    let metrics = cdp
        .call(Some(session), "Page.getLayoutMetrics", json!({}), deadline)
        .await?;
    let viewport = &metrics["cssVisualViewport"];
    let width = viewport["clientWidth"].as_f64().unwrap_or(0.);
    let height = viewport["clientHeight"].as_f64().unwrap_or(0.);
    if !width.is_finite()
        || !height.is_finite()
        || width < 1.
        || height < 1.
        || width > 1920.
        || height > 1920.
    {
        return Err(Failure::new("browser_image_too_large"));
    }
    let image = cdp.call(Some(session), "Page.captureScreenshot", json!({
        "format":"jpeg","quality":60,"captureBeyondViewport":false,
        "clip":{"x":viewport["pageX"],"y":viewport["pageY"],"width":width,"height":height,"scale":1},
    }), deadline).await?;
    let data = image["data"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= MAX_ENCODED)
        .ok_or_else(|| Failure::new("browser_image_too_large"))?;
    Ok(
        json!({"mime_type":"image/jpeg","data":data,"width":width.round() as u64,
        "height":height.round() as u64,"css_pixel_scale":1,"scope":"viewport"}),
    )
}
