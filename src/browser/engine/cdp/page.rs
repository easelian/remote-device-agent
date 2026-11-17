use super::super::programs::{COLLECT, DETAILS, MARKS, SELECT, SET};
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
    pub(super) async fn world(&self, frame: &Frame, deadline: Instant) -> Result<Value> {
        let session = self.session(&frame.session)?;
        let id = self.key(&frame.id.0)?;
        self.check(&frame.document.0)?;
        let result = self
            .wire
            .call(
                Some(session),
                Command::PageCreateIsolatedWorld,
                json!({"frameId":id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}),
                deadline,
            )
            .await?;
        result
            .get("executionContextId")
            .filter(|v| v.is_i64())
            .cloned()
            .ok_or_else(stale)
    }
    async fn object(&self, frame: &Frame, node: &Node, deadline: Instant) -> Result<String> {
        if frame.session != node.context {
            return Err(stale());
        }
        let context = self.world(frame, deadline).await?;
        let result=self.wire.call(Some(self.session(&frame.session)?),Command::DomResolveNode,json!({"backendNodeId":self.node(node)?,"executionContextId":context,"objectGroup":"rdh-observe"}),deadline).await?;
        result["object"]["objectId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(stale)
    }
    async fn release_object(&self, context: &Context, object: &str, deadline: Instant) {
        if let Ok(session) = self.session(context) {
            let _ = self
                .wire
                .cleanup()
                .call(
                    Some(session),
                    Command::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    deadline,
                )
                .await;
        }
    }
    async fn reader_call(
        &self,
        reader: &Scan,
        function: &str,
        args: Value,
        deadline: Instant,
    ) -> Result<Value> {
        let lease = self.scan(reader)?;
        self.wire.call(Some(self.session(&lease.context)?),Command::RuntimeCallFunction(Effect::None),json!({"objectId":lease.object,"functionDeclaration":function,"arguments":args,"returnByValue":true}),deadline).await
    }
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
        let mut todo = VecDeque::from([&tree["frameTree"]]);
        let mut frames = Vec::new();
        while let Some(node) = todo.pop_front() {
            if frames.len() >= 32 {
                return Err(Failure::new("browser_frame_limit"));
            }
            let frame = &node["frame"];
            if let (Some(id), Some(document)) = (frame["id"].as_str(), frame["loaderId"].as_str()) {
                frames.push(Frame {
                    session: context.clone(),
                    id: FrameId(a.handle(id)),
                    document: DocumentId(a.handle(document)),
                    url: frame["url"].as_str().unwrap_or("").into(),
                    fragment: frame["urlFragment"].as_str().unwrap_or("").into(),
                    ancestors: Vec::new(),
                });
            } else {
                return Err(stale());
            }
            if let Some(children) = node["childFrames"].as_array() {
                todo.extend(children);
            }
        }
        Ok(frames)
    }
    async fn metadata(&self, frame: &Frame, deadline: Instant) -> Result<Value> {
        let a = &self.backend;
        let context = a.world(frame, deadline).await?;
        let result=a.wire.call(Some(a.session(&frame.session)?),Command::RuntimeEvaluate(Effect::None),json!({"contextId":context,"returnByValue":true,"expression":"({url:location.href.slice(0,4096),title:document.title.slice(0,512),ready_state:document.readyState})"}),deadline).await?;
        let value = script_value(result)?;
        if !value["url"].is_string() || !value["title"].is_string() {
            return Err(stale());
        }
        Ok(value)
    }
    async fn open_reader(
        &self,
        frame: &Frame,
        options: ReaderOptions,
        deadline: Instant,
    ) -> Result<Scan> {
        let a = &self.backend;
        let context = a.world(frame, deadline).await?;
        let group = format!("rdh-reader-{}", a.key(&frame.id.0)?);
        let session = Some(a.session(&frame.session)?);
        a.wire
            .call(
                session,
                Command::RuntimeReleaseObjectGroup,
                json!({"objectGroup":group}),
                deadline,
            )
            .await?;
        let options = json!({"interactive":options.interactive,"viewport":options.viewport,"find":options.find,"limit":options.limit});
        let created=a.wire.call(session,Command::RuntimeEvaluate(Effect::None),json!({"contextId":context,"objectGroup":group,"expression":format!("({COLLECT})({MARKS},{options})")}),deadline).await?;
        if created.get("exceptionDetails").is_some() {
            return Err(stale());
        }
        let object = created["result"]["objectId"]
            .as_str()
            .ok_or_else(stale)?
            .to_owned();
        Ok(Scan(Arc::new(ScanLease {
            context: frame.session.clone(),
            object,
            group,
            closed: AtomicBool::new(false),
        })))
    }
    async fn reader_status(&self, reader: &Scan, deadline: Instant) -> Result<ReaderStatus> {
        status(
            self.backend
                .reader_call(
                    reader,
                    "function(){return this.status();}",
                    json!([]),
                    deadline,
                )
                .await?,
        )
    }
    async fn collect(
        &self,
        reader: &Scan,
        nodes: usize,
        scan: usize,
        deadline: Instant,
    ) -> Result<Batch> {
        let value = script_value(
            self.backend
                .reader_call(
                    reader,
                    "function(nodes,scan){return this.collect(nodes,scan);}",
                    json!([{"value":nodes},{"value":scan}]),
                    deadline,
                )
                .await?,
        )?;
        let candidates = value["nodes"].as_array().ok_or_else(stale)?;
        if candidates.len() > nodes
            || candidates.iter().any(|n| {
                n["index"].as_u64().is_none() && (!n["name"].is_string() || !n["role"].is_string())
            })
        {
            return Err(stale());
        }
        let changed = value["changed"].as_bool().ok_or_else(stale)?;
        let scanned = value["scanned"].as_u64().ok_or_else(stale)? as usize;
        let matches = value["matches"].as_u64().ok_or_else(stale)? as usize;
        let omitted = value["omitted"]
            .as_array()
            .ok_or_else(stale)?
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(stale))
            .collect::<Result<_>>()?;
        Ok(Batch {
            nodes: candidates.clone(),
            changed,
            scanned,
            matches,
            omitted,
        })
    }
    async fn ack(
        &self,
        reader: &Scan,
        count: usize,
        chars: usize,
        deadline: Instant,
    ) -> Result<ReaderStatus> {
        status(
            self.backend
                .reader_call(
                    reader,
                    "function(count,chars){return this.ack(count,chars);}",
                    json!([{"value":count},{"value":chars}]),
                    deadline,
                )
                .await?,
        )
    }
    async fn close_reader(&self, reader: &Scan, deadline: Instant) {
        let a = &self.backend;
        // Revoke the local lease even when native cleanup fails or times out.
        if a.check(&reader.0.context.0).is_err() || reader.0.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let session = Some(reader.0.context.0.key.as_ref());
        let _=a.wire.cleanup().call(session,Command::RuntimeCallFunction(Effect::None),json!({"objectId":reader.0.object,"functionDeclaration":"function(){this.close();}"}),deadline).await;
        let _ = a
            .wire
            .cleanup()
            .call(
                session,
                Command::RuntimeReleaseObjectGroup,
                json!({"objectGroup":reader.0.group}),
                deadline,
            )
            .await;
    }
    async fn release_scratch(&self, context: &Context, deadline: Instant) {
        let a = &self.backend;
        if let Ok(session) = a.session(context) {
            let _ = a
                .wire
                .cleanup()
                .call(
                    Some(session),
                    Command::RuntimeReleaseObjectGroup,
                    json!({"objectGroup":"rdh-observe"}),
                    deadline,
                )
                .await;
        }
    }
    async fn locate(
        &self,
        frame: &Frame,
        reader: &Scan,
        index: u64,
        deadline: Instant,
    ) -> Result<Option<Found>> {
        let a = &self.backend;
        let lease = a.scan(reader)?;
        if lease.context != frame.session {
            return Err(stale());
        }
        let session = Some(a.session(&frame.session)?);
        let remote=a.wire.call(session,Command::RuntimeCallFunction(Effect::None),json!({"objectId":lease.object,"functionDeclaration":"function(i){return this.pending[i].node;}","arguments":[{"value":index}],"objectGroup":"rdh-observe"}),deadline).await?;
        if remote.get("exceptionDetails").is_some() {
            return Err(stale());
        }
        let object = remote["result"]["objectId"].as_str().ok_or_else(stale)?;
        let dom = a
            .wire
            .call(
                session,
                Command::DomDescribeNode,
                json!({"objectId":object,"depth":0}),
                deadline,
            )
            .await?;
        let key = dom["node"]["backendNodeId"].as_i64().ok_or_else(stale)?;
        let node = Node {
            context: frame.session.clone(),
            key,
        };
        let ax = self.accessibility(&node, deadline).await?;
        let exposed = ax.into_iter().find(|n| n.exposed);
        let (role, name, value) = exposed
            .as_ref()
            .map(|n| (n.role.clone(), n.name.clone(), n.value.clone()))
            .unwrap_or_default();
        Ok(Some(Found {
            node,
            exposed: exposed.is_some(),
            role,
            name,
            value,
            data: None,
        }))
    }
    async fn details_of(&self, reader: &Scan, indexes: &[u64], deadline: Instant) -> Result<Value> {
        if indexes.is_empty() {
            return Ok(Value::Null);
        }
        let reply=self.backend.reader_call(reader,&format!("function(indexes){{const details={DETAILS};return indexes.map(i=>{{try{{return details.call(this.pending[i].node);}}catch(error){{return null;}}}});}}"),json!([{"value":indexes}]),deadline).await?;
        let value = script_value(reply)?;
        if value.as_array().is_none_or(|v| v.len() != indexes.len()) {
            return Err(stale());
        }
        Ok(value)
    }
}

impl PageActionPort for Engine {
    async fn details(&self, frame: &Frame, node: &Node, deadline: Instant) -> Result<Value> {
        let a = &self.backend;
        let object = a.object(frame, node, deadline).await?;
        let reply = a
            .wire
            .call(
                Some(a.session(&frame.session)?),
                Command::RuntimeCallFunction(Effect::None),
                json!({"objectId":object,"functionDeclaration":DETAILS,"returnByValue":true}),
                deadline,
            )
            .await;
        a.release_object(&frame.session, &object, deadline).await;
        let value = script_value(reply?)?;
        if !value.is_object() {
            return Err(stale());
        }
        Ok(value)
    }
    async fn accessibility(&self, node: &Node, deadline: Instant) -> Result<Vec<Accessible>> {
        let a = &self.backend;
        let key = a.node(node)?;
        let ax = a
            .wire
            .call(
                Some(a.session(&node.context)?),
                Command::AccessibilityGetPartialAXTree,
                json!({"backendNodeId":key,"fetchRelatives":false}),
                deadline,
            )
            .await?;
        let nodes = ax["nodes"].as_array().ok_or_else(stale)?;
        Ok(nodes
            .iter()
            .filter(|n| n["backendDOMNodeId"].as_i64() == Some(key))
            .map(|n| Accessible {
                exposed: n["ignored"] != true,
                role: n["role"]["value"].as_str().unwrap_or("").into(),
                name: n["name"]["value"].as_str().unwrap_or("").into(),
                value: n["value"]["value"].clone(),
            })
            .collect())
    }
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
    async fn scroll(&self, frame: &Frame, delta: f64, deadline: Instant) -> Result<Point> {
        if !delta.is_finite() {
            return Err(Failure::new("browser_invalid_params"));
        }
        let a = &self.backend;
        let context = a.world(frame, deadline).await?;
        let result=a.wire.call(Some(a.session(&frame.session)?),Command::RuntimeEvaluate(Effect::Page),json!({"contextId":context,"returnByValue":true,"expression":format!("(()=>{{window.scrollBy(0,{delta});return {{scroll_x:window.scrollX,scroll_y:window.scrollY}};}})()")}),deadline).await?;
        let value = script_value(result).map_err(|_| Failure::unknown("browser_not_actionable"))?;
        let decode = || Failure::unknown("browser_not_actionable");
        Ok(Point {
            x: value["scroll_x"].as_f64().ok_or_else(decode)?,
            y: value["scroll_y"].as_f64().ok_or_else(decode)?,
        })
    }
    async fn scroll_into_view(&self, node: &Node, deadline: Instant) -> Result<()> {
        self.backend
            .node_command(node, Command::DomScrollIntoViewIfNeeded, deadline)
            .await
    }
    async fn focus(&self, node: &Node, deadline: Instant) -> Result<()> {
        self.backend
            .node_command(node, Command::DomFocus, deadline)
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
    async fn select_all(&self, context: &Context, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        let modifiers = if cfg!(target_os = "macos") { 4 } else { 2 };
        let mut press = json!({"type":"keyDown","key":"a","code":"KeyA","modifiers":modifiers,"windowsVirtualKeyCode":65});
        // macOS CDP key events need an explicit native editing command; Meta+A
        // alone does not perform the platform shortcut.
        if cfg!(target_os = "macos") {
            press["commands"] = json!(["selectAll"]);
        }
        a.wire.input_pair(Some(a.session(context)?),InputPair::Key,press,json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":modifiers,"windowsVirtualKeyCode":65}),deadline).await
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
    async fn insert_text(&self, context: &Context, text: &str, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .call(
                Some(a.session(context)?),
                Command::InputInsertText,
                json!({"text":text}),
                deadline,
            )
            .await
            .map(drop)
    }
    async fn select(
        &self,
        frame: &Frame,
        node: &Node,
        value: &str,
        deadline: Instant,
    ) -> Result<bool> {
        let a = &self.backend;
        let object = a.object(frame, node, deadline).await?;
        let result=a.wire.call(Some(a.session(&frame.session)?),Command::RuntimeCallFunction(Effect::Page),json!({"objectId":object,"functionDeclaration":SELECT,"arguments":[{"value":value}],"returnByValue":true}),deadline).await;
        a.release_object(&frame.session, &object, deadline).await;
        Ok(script_value(result?).map_err(|_| Failure::unknown("browser_not_actionable"))? == true)
    }
    async fn set_form_value(
        &self,
        frame: &Frame,
        node: &Node,
        value: &str,
        apply: bool,
        deadline: Instant,
    ) -> Result<Option<bool>> {
        let a = &self.backend;
        let object = a.object(frame, node, deadline).await?;
        let result=a.wire.call(Some(a.session(&frame.session)?),Command::RuntimeCallFunction(if apply{Effect::Page}else{Effect::None}),json!({"objectId":object,"functionDeclaration":SET,"arguments":[{"value":value},{"value":apply}],"returnByValue":true}),deadline).await;
        a.release_object(&frame.session, &object, deadline).await;
        Ok(script_value(result?)
            .map_err(|mut e| {
                if apply {
                    e.state = "unknown";
                }
                e
            })?
            .as_bool())
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
