use super::*;

impl Engine {
    pub async fn visual_viewport(&self, context: &Context, deadline: Instant) -> Result<Viewport> {
        let metrics = self.backend.layout_metrics(context, deadline).await?;
        let view = &metrics["cssVisualViewport"];
        Ok(Viewport {
            x: view["pageX"].as_f64().unwrap_or(0.),
            y: view["pageY"].as_f64().unwrap_or(0.),
            width: view["clientWidth"].as_f64().unwrap_or(0.),
            height: view["clientHeight"].as_f64().unwrap_or(0.),
        })
    }
    pub async fn capture_viewport(
        &self,
        context: &Context,
        view: &Viewport,
        deadline: Instant,
    ) -> Result<String> {
        let a = &self.backend;
        let image=a.wire.call(Some(a.session(context)?),Command::PageCaptureScreenshot,json!({"format":"jpeg","quality":60,"captureBeyondViewport":false,"clip":{"x":view.x,"y":view.y,"width":view.width,"height":view.height,"scale":1}}),deadline).await?;
        image["data"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Failure::new("browser_image_too_large"))
    }
}
