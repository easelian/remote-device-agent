use super::*;
use std::collections::VecDeque;

impl Engine {
    /// Flatten the native renderer topology into one semantic page tree. Page
    /// policy never needs to know which documents share a renderer connection.
    pub async fn page_frames(&self, context: &Context, deadline: Instant) -> Result<Vec<Frame>> {
        let mut frames = self.frames(context, deadline).await?;
        let mut remaining: Vec<_> = self
            .backend
            .children
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut selected =
            std::collections::HashMap::from([(context.clone(), Vec::<(Context, FrameId)>::new())]);
        loop {
            let Some(index) = remaining
                .iter()
                .position(|t| t.parent.as_ref().is_some_and(|p| selected.contains_key(p)))
            else {
                break;
            };
            let child = remaining.remove(index);
            let parent = child.parent.as_ref().unwrap();
            let mut ancestors = vec![(parent.clone(), child.root.clone())];
            ancestors.extend(selected.get(parent).unwrap().iter().cloned());
            selected.insert(child.context.clone(), ancestors.clone());
            for mut frame in self.frames(&child.context, deadline).await? {
                frame.ancestors = ancestors.clone();
                if let Some(old) = frames.iter_mut().find(|f| f.id == frame.id) {
                    if frame.parent.is_none() {
                        frame.parent = old.parent.clone();
                    }
                    *old = frame;
                } else {
                    frames.push(frame);
                }
            }
        }
        Ok(frames)
    }
}

impl Adapter {
    pub(super) async fn layout_metrics(
        &self,
        context: &Context,
        deadline: Instant,
    ) -> Result<Value> {
        self.wire
            .call(
                Some(self.session(context)?),
                Command::PageGetLayoutMetrics,
                json!({}),
                deadline,
            )
            .await
    }
    async fn node_command(&self, node: &Node, command: Command, deadline: Instant) -> Result<()> {
        self.wire
            .call(
                Some(self.session(&node.context)?),
                command,
                json!({"backendNodeId":self.node(node)?}),
                deadline,
            )
            .await
            .map(drop)
    }
}

impl PageReadPort for Engine {
    async fn frames(&self, context: &Context, deadline: Instant) -> Result<Vec<Frame>> {
        let a = &self.backend;
        let tree = a
            .wire
            .call(
                Some(a.session(context)?),
                Command::PageGetFrameTree,
                json!({}),
                deadline,
            )
            .await?;
        let mut todo = VecDeque::from([(&tree["frameTree"], None)]);
        let mut frames = Vec::new();
        while let Some((node, inherited)) = todo.pop_front() {
            if frames.len() >= 32 {
                return Err(Failure::new("browser_frame_limit"));
            }
            let frame = &node["frame"];
            if let (Some(id), Some(document)) = (frame["id"].as_str(), frame["loaderId"].as_str()) {
                frames.push(Frame {
                    session: context.clone(),
                    id: FrameId(a.handle(id)),
                    parent: frame["parentId"]
                        .as_str()
                        .or(inherited)
                        .map(|p| FrameId(a.handle(p))),
                    document: DocumentId(a.handle(document)),
                    ancestors: Vec::new(),
                });
            } else {
                return Err(stale());
            }
            if let Some(children) = node["childFrames"].as_array() {
                todo.extend(children.iter().map(|child| (child, frame["id"].as_str())));
            }
        }
        Ok(frames)
    }
}

impl PageActionPort for Engine {
    async fn layout(&self, context: &Context, deadline: Instant) -> Result<Viewport> {
        let metrics = self.backend.layout_metrics(context, deadline).await?;
        viewport(&metrics["cssLayoutViewport"])
    }
    async fn frame_owner(
        &self,
        context: &Context,
        frame: &FrameId,
        deadline: Instant,
    ) -> Result<Node> {
        let a = &self.backend;
        let owner = a
            .wire
            .call(
                Some(a.session(context)?),
                Command::DomGetFrameOwner,
                json!({"frameId":a.key(&frame.0)?}),
                deadline,
            )
            .await?;
        Ok(Node {
            context: context.clone(),
            key: owner["backendNodeId"].as_i64().ok_or_else(actionable)?,
        })
    }
    async fn quad(&self, node: &Node, deadline: Instant) -> Result<[f64; 8]> {
        let a = &self.backend;
        let quads = a
            .wire
            .call(
                Some(a.session(&node.context)?),
                Command::DomGetContentQuads,
                json!({"backendNodeId":a.node(node)?}),
                deadline,
            )
            .await?;
        let q = quads["quads"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(Value::as_array)
            .filter(|v| v.len() == 8)
            .ok_or_else(actionable)?;
        let mut points = [0.; 8];
        for (point, value) in points.iter_mut().zip(q) {
            *point = value
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or_else(actionable)?;
        }
        Ok(points)
    }
    async fn hit_at(&self, context: &Context, point: Point, deadline: Instant) -> Result<Hit> {
        let view = self.layout(context, deadline).await?;
        let Point { x, y } = point;
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.
            || y < 0.
            || x >= view.width
            || y >= view.height
        {
            return Err(actionable());
        }
        let a = &self.backend;
        let value = a
            .wire
            .call(
                Some(a.session(context)?),
                Command::DomGetNodeForLocation,
                json!({"x":(x+view.x).round() as i64,"y":(y+view.y).round() as i64}),
                deadline,
            )
            .await?;
        Ok(Hit {
            frame: FrameId(a.handle(value["frameId"].as_str().ok_or_else(actionable)?)),
            node: Node {
                context: context.clone(),
                key: value["backendNodeId"].as_i64().ok_or_else(actionable)?,
            },
        })
    }
    async fn scroll_into_view(&self, node: &Node, deadline: Instant) -> Result<()> {
        self.backend
            .node_command(node, Command::DomScrollIntoViewIfNeeded, deadline)
            .await
    }
    async fn click(&self, context: &Context, point: Point, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        let Point { x, y } = point;
        a.wire
            .input_pair(
                Some(a.session(context)?),
                InputPair::Mouse,
                json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
                json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
                deadline,
            )
            .await
    }
    async fn press(&self, context: &Context, key: &str, deadline: Instant) -> Result<()> {
        let code = match key {
            "Enter" => 13,
            "Tab" => 9,
            "Escape" => 27,
            "Backspace" => 8,
            "ArrowDown" => 40,
            "ArrowUp" => 38,
            "ArrowLeft" => 37,
            "ArrowRight" => 39,
            _ => return Err(Failure::new("browser_invalid_params")),
        };
        let a = &self.backend;
        a.wire.input_pair(Some(a.session(context)?),InputPair::Key,json!({"type":"keyDown","key":key,"windowsVirtualKeyCode":code,"text":if key=="Enter"{"\r"}else{""}}),json!({"type":"keyUp","key":key,"windowsVirtualKeyCode":code}),deadline).await
    }
    async fn attach_file(&self, node: &Node, path: &Path, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .call(
                Some(a.session(&node.context)?),
                Command::DomSetFileInputFiles,
                json!({"backendNodeId":a.node(node)?,"files":[path]}),
                deadline,
            )
            .await
            .map(drop)
    }
}

pub(super) fn viewport(value: &Value) -> Result<Viewport> {
    let number = |name: &str| {
        value[name]
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(actionable)
    };
    Ok(Viewport {
        x: number("pageX")?,
        y: number("pageY")?,
        width: number("clientWidth")?,
        height: number("clientHeight")?,
    })
}
