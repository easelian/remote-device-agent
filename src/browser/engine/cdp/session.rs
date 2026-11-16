use super::*;
use base64::Engine as _;

pub(in crate::browser::engine) struct Events {
    raw: tokio::sync::broadcast::Receiver<Value>,
    adapter: Adapter,
}
impl Events {
    pub(in crate::browser::engine) async fn recv(&mut self) -> Result<Event> {
        loop {
            let raw = match self.raw.recv().await {
                Ok(value) => value,
                Err(_) => {
                    self.adapter.stop();
                    return Err(Failure::unknown("browser_lost"));
                }
            };
            match decode(&self.adapter, raw) {
                Ok(Some(event)) => return Ok(event),
                Ok(None) => {}
                Err(error) => {
                    self.adapter.stop();
                    return Err(error);
                }
            }
        }
    }
}
fn info(a: &Adapter, value: &Value) -> TargetInfo {
    let id = value["targetId"].as_str().unwrap_or("");
    TargetInfo {
        tab: TabId(a.handle(id)),
        root: FrameId(a.handle(id)),
        kind: match value["type"].as_str() {
            Some("page") => ContextKind::Page,
            Some("iframe") => ContextKind::Frame,
            _ => ContextKind::Background,
        },
        url: value["url"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(4096)
            .collect(),
        title: value["title"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(512)
            .collect(),
    }
}
fn decode(a: &Adapter, event: Value) -> Result<Option<Event>> {
    let params = &event["params"];
    let context = event["sessionId"].as_str().map(|s| Context(a.handle(s)));
    let e = match event["method"].as_str().unwrap_or("") {
        "Target.attachedToTarget" => {
            let attached = Context(
                a.handle(
                    params["sessionId"]
                        .as_str()
                        .ok_or_else(|| Failure::unknown("browser_lost"))?,
                ),
            );
            let info = info(a, &params["targetInfo"]);
            if info.kind == ContextKind::Frame {
                let mut children = a.children.lock().unwrap();
                if children.len() < 40 {
                    children.insert(
                        attached.clone(),
                        Attachment {
                            context: attached.clone(),
                            parent: context,
                            root: info.root.clone(),
                        },
                    );
                }
            }
            Event::Attached {
                context: attached,
                info,
            }
        }
        "Target.detachedFromTarget" => {
            let detached = Context(a.handle(params["sessionId"].as_str().unwrap_or("")));
            a.children.lock().unwrap().remove(&detached);
            Event::Detached(detached)
        }
        "Target.targetInfoChanged" => Event::TargetChanged(info(a, &params["targetInfo"])),
        "Fetch.requestPaused" => Event::DocumentPaused(DocumentRequest {
            handle: Request {
                context: context.clone(),
                key: params["requestId"]
                    .as_str()
                    .ok_or_else(|| Failure::unknown("browser_lost"))?
                    .into(),
            },
            network: NetworkId {
                context: context.clone(),
                key: params["networkId"].as_str().unwrap_or("").into(),
            },
            context,
            frame: FrameId(a.handle(params["frameId"].as_str().unwrap_or(""))),
            url: params["request"]["url"].as_str().unwrap_or("").into(),
            responded: params.get("responseStatusCode").is_some()
                || params.get("responseErrorReason").is_some(),
            redirected: params.get("redirectedRequestId").is_some(),
            status: params["responseStatusCode"].as_u64(),
            headers: params["responseHeaders"].as_array().map(|headers| {
                headers
                    .iter()
                    .filter_map(|h| {
                        Some(Header {
                            name: h["name"].as_str()?.into(),
                            value: h["value"].as_str()?.into(),
                        })
                    })
                    .collect()
            }),
        }),
        "Network.responseReceivedExtraInfo" => {
            let (Some(context), Some(request), Some(space)) = (
                context,
                params["requestId"].as_str(),
                params["resourceIPAddressSpace"].as_str(),
            ) else {
                return Ok(None);
            };
            Event::Connection {
                network: NetworkId {
                    context: Some(context),
                    key: request.into(),
                },
                public: space == "Public",
            }
        }
        "Page.javascriptDialogOpening" => Event::DialogOpened {
            context,
            kind: params["type"].clone(),
            message: params["message"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(512)
                .collect(),
        },
        "Page.javascriptDialogClosed" => Event::DialogClosed { context },
        _ => return Ok(None),
    };
    Ok(Some(e))
}
impl Adapter {
    fn request_session<'a>(&self, request: &'a Request) -> Result<Option<&'a str>> {
        request
            .context
            .as_ref()
            .map(|s| self.session(s))
            .transpose()
    }
}

impl Engine {
    pub fn events(&self) -> super::super::Events {
        super::super::Events {
            backend: Events {
                raw: self.backend.wire.events(),
                adapter: self.backend.clone(),
            },
        }
    }
    pub async fn product(&self, deadline: Instant) -> Result<Option<String>> {
        let reply = self
            .backend
            .wire
            .call(None, Command::BrowserGetVersion, json!({}), deadline)
            .await?;
        Ok(reply["product"].as_str().map(str::to_owned))
    }
    pub async fn initialize(&self, deadline: Instant) -> Result<()> {
        let wire = &self.backend.wire;
        wire.call(
            None,
            Command::BrowserSetDownloadBehavior,
            json!({"behavior":"deny","eventsEnabled":true}),
            deadline,
        )
        .await?;
        wire.call(
            None,
            Command::TargetSetDiscoverTargets,
            json!({"discover":true}),
            deadline,
        )
        .await?;
        wire.call(
            None,
            Command::TargetSetAutoAttach,
            json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),
            deadline,
        )
        .await?;
        Ok(())
    }
    pub async fn new_tab(&self, deadline: Instant) -> Result<TabId> {
        let result = self
            .backend
            .wire
            .call(
                None,
                Command::TargetCreateTarget,
                json!({"url":"about:blank"}),
                deadline,
            )
            .await?;
        Ok(TabId(
            self.backend.handle(
                result["targetId"]
                    .as_str()
                    .ok_or_else(|| Failure::unknown("browser_command_failed"))?,
            ),
        ))
    }
    pub async fn close_tab(&self, tab: &TabId, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .call(
                None,
                Command::TargetCloseTarget,
                json!({"targetId":a.key(&tab.0)?}),
                deadline,
            )
            .await
            .map(drop)
    }
    pub async fn activate(&self, context: &Context, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .call(
                Some(a.session(context)?),
                Command::PageBringToFront,
                json!({}),
                deadline,
            )
            .await
            .map(drop)
    }
    pub async fn navigate(
        &self,
        context: &Context,
        url: &str,
        deadline: Instant,
    ) -> Result<Navigation> {
        let a = &self.backend;
        let reply = a
            .wire
            .call(
                Some(a.session(context)?),
                Command::PageNavigate,
                json!({"url":url}),
                deadline,
            )
            .await?;
        Ok(Navigation {
            failed: reply.get("errorText").is_some(),
        })
    }
    pub async fn stop_loading(&self, context: &Context, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .cleanup()
            .call(
                Some(a.session(context)?),
                Command::PageStopLoading,
                json!({}),
                deadline,
            )
            .await
            .map(drop)
    }
    pub async fn dialog(&self, context: &Context, accept: bool, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .call(
                Some(a.session(context)?),
                Command::PageHandleJavaScriptDialog,
                json!({"accept":accept}),
                deadline,
            )
            .await
            .map(drop)
    }
    pub async fn wheel(
        &self,
        context: &Context,
        point: Point,
        delta: f64,
        deadline: Instant,
    ) -> Result<()> {
        let a = &self.backend;
        a.wire
            .call(
                Some(a.session(context)?),
                Command::InputMouseWheel,
                json!({"type":"mouseWheel","x":point.x,"y":point.y,"deltaX":0,"deltaY":delta}),
                deadline,
            )
            .await
            .map(drop)
    }
}

impl SessionPort for Engine {
    fn connected(&self) -> bool {
        Engine::connected(self)
    }
    fn stop(&self) {
        Engine::stop(self)
    }
    async fn initialize_context(
        &self,
        context: &Context,
        downloads: bool,
        responses: bool,
        deadline: Instant,
    ) -> Result<()> {
        let a = &self.backend;
        let wire = a.wire.cleanup();
        let session = Some(a.session(context)?);
        let mut patterns =
            vec![json!({"urlPattern":"*","resourceType":"Document","requestStage":"Request"})];
        if responses {
            patterns.push(
                json!({"urlPattern":"*","resourceType":"Document","requestStage":"Response"}),
            );
        }
        wire.call(
            session,
            Command::FetchEnable,
            json!({"patterns":patterns}),
            deadline,
        )
        .await?;
        wire.call(
            session,
            Command::TargetSetAutoAttach,
            json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),
            deadline,
        )
        .await?;
        self.resume_context(context, deadline).await?;
        wire.call(session,Command::NetworkSetBlockedURLs,json!({"urls":["file:*","filesystem:*","javascript:*","chrome:*","chrome-extension:*","devtools:*","mailto:*","tel:*"]}),deadline).await?;
        if downloads {
            wire.call(
                session,
                Command::NetworkSetCacheDisabled,
                json!({"cacheDisabled":true}),
                deadline,
            )
            .await?;
        }
        wire.call(
            session,
            Command::PageSetInterceptFileChooserDialog,
            json!({"enabled":true}),
            deadline,
        )
        .await?;
        for command in [
            Command::PageEnable,
            Command::RuntimeEnable,
            Command::NetworkEnable,
            Command::AccessibilityEnable,
        ] {
            wire.call(session, command, json!({}), deadline).await?;
        }
        Ok(())
    }
    async fn resume_context(&self, context: &Context, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .cleanup()
            .call(
                Some(a.session(context)?),
                Command::RuntimeRunIfWaitingForDebugger,
                json!({}),
                deadline,
            )
            .await
            .map(drop)
    }
    async fn discard_tab(&self, tab: &TabId, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        a.wire
            .cleanup()
            .call(
                None,
                Command::TargetCloseTarget,
                json!({"targetId":a.key(&tab.0)?}),
                deadline,
            )
            .await
            .map(drop)
    }
    async fn respond(
        &self,
        request: &Request,
        decision: RequestDecision,
        deadline: Instant,
    ) -> Result<()> {
        let a = &self.backend;
        let (command, params) = match decision {
            RequestDecision::Continue => (
                Command::FetchContinueRequest,
                json!({"requestId":request.key}),
            ),
            RequestDecision::Deny => (
                Command::FetchFailRequest,
                json!({"requestId":request.key,"errorReason":"BlockedByClient"}),
            ),
            RequestDecision::Abort => (
                Command::FetchFailRequest,
                json!({"requestId":request.key,"errorReason":"Aborted"}),
            ),
        };
        a.wire
            .cleanup()
            .call(a.request_session(request)?, command, params, deadline)
            .await
            .map(drop)
    }
}

impl TransferPort for Engine {
    async fn open_stream(&self, request: &Request, deadline: Instant) -> Result<Stream> {
        let a = &self.backend;
        let value = a
            .wire
            .cleanup()
            .call(
                a.request_session(request)?,
                Command::FetchTakeResponseBodyAsStream,
                json!({"requestId":request.key}),
                deadline,
            )
            .await?;
        Ok(Stream {
            context: request.context.clone(),
            key: value["stream"]
                .as_str()
                .ok_or_else(|| Failure::new("browser_file_incomplete"))?
                .into(),
        })
    }
    async fn read_stream(&self, stream: &Stream, limit: usize, deadline: Instant) -> Result<Chunk> {
        let a = &self.backend;
        let session = stream.context.as_ref().map(|s| a.session(s)).transpose()?;
        let data = a
            .wire
            .cleanup()
            .call(
                session,
                Command::IoRead,
                json!({"handle":stream.key,"size":limit}),
                deadline,
            )
            .await?;
        let encoded = data["data"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_file_incomplete"))?;
        if encoded.len() > limit * 2 {
            return Err(Failure::new("browser_file_limit"));
        }
        let bytes = if data["base64Encoded"] == true {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Failure::new("browser_file_incomplete"))?
        } else {
            encoded.as_bytes().to_vec()
        };
        if bytes.len() > limit {
            return Err(Failure::new("browser_file_limit"));
        }
        Ok(Chunk {
            bytes,
            eof: data["eof"] == true,
        })
    }
    async fn close_stream(&self, stream: Stream, deadline: Instant) -> Result<()> {
        let a = &self.backend;
        let session = stream.context.as_ref().map(|s| a.session(s)).transpose()?;
        a.wire
            .cleanup()
            .call(
                session,
                Command::IoClose,
                json!({"handle":stream.key}),
                deadline,
            )
            .await
            .map(drop)
    }
}
