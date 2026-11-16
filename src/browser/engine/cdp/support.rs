use super::super::programs::{FOCUSED, INSERT};
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
    pub async fn focused_node(&self, frame: &Frame, deadline: Instant) -> Result<Option<Node>> {
        let a = &self.backend;
        let context = a.world(frame, deadline).await?;
        let session = Some(a.session(&frame.session)?);
        let value = a
            .wire
            .call(
                session,
                Command::RuntimeEvaluate(Effect::None),
                json!({"contextId":context,"expression":FOCUSED,"objectGroup":"rdh-human-focus"}),
                deadline,
            )
            .await?;
        if value.get("exceptionDetails").is_some() {
            return Err(Failure::new("browser_stale_frame"));
        }
        let Some(object) = value["result"]["objectId"].as_str() else {
            return Ok(None);
        };
        let node = a
            .wire
            .call(
                session,
                Command::DomDescribeNode,
                json!({"objectId":object,"depth":0}),
                deadline,
            )
            .await;
        let _ = a
            .wire
            .cleanup()
            .call(
                session,
                Command::RuntimeReleaseObjectGroup,
                json!({"objectGroup":"rdh-human-focus"}),
                deadline,
            )
            .await;
        Ok(Some(Node {
            context: frame.session.clone(),
            key: node?["node"]["backendNodeId"]
                .as_i64()
                .ok_or_else(|| Failure::new("browser_stale_frame"))?,
        }))
    }
    pub async fn insert_at_focus(
        &self,
        frame: &Frame,
        node: &Node,
        text: &str,
        deadline: Instant,
    ) -> Result<FocusOutcome> {
        let a = &self.backend;
        if frame.session != node.context {
            return Err(Failure::new("browser_stale_frame"));
        }
        let session = Some(a.session(&frame.session)?);
        let context = a.world(frame, deadline).await?;
        let node=a.wire.call(session,Command::DomResolveNode,json!({"backendNodeId":a.node(node)?,"executionContextId":context,"objectGroup":"rdh-human-focus"}),deadline).await?;
        let object = node["object"]["objectId"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_stale_frame"))?;
        let result=a.wire.call(session,Command::RuntimeCallFunction(Effect::Page),json!({"objectId":object,"functionDeclaration":INSERT,"arguments":[{"value":text}],"returnByValue":true}),deadline).await;
        let _ = a
            .wire
            .cleanup()
            .call(
                session,
                Command::RuntimeReleaseObjectGroup,
                json!({"objectGroup":"rdh-human-focus"}),
                deadline,
            )
            .await;
        let result = result.map_err(|mut e| {
            if e.state != "not_sent" {
                e.state = "unknown";
            }
            e
        })?;
        Ok(
            match script_value(result).ok().as_ref().and_then(Value::as_str) {
                Some("inserted") => FocusOutcome::Inserted,
                Some("stale") => FocusOutcome::Stale,
                _ => FocusOutcome::Changed,
            },
        )
    }
}
