//! Bounded, document-bound semantic observations and typed element actions.
use super::{Failure, Result, cdp::Cdp};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Frame {
    pub session: String,
    pub id: String,
    pub loader: String,
    pub url: String,
    pub ancestors: Vec<(String, String)>,
}
pub struct Element {
    frame: Frame,
    backend: i64,
    signature: String,
    role: String,
    name: String,
    choices: Vec<String>,
}

fn clipped(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
pub fn url_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.username().is_empty() && u.password().is_none()
    })
}

pub async fn frame_tree(cdp: &Cdp, session: &str, deadline: Instant) -> Result<Vec<Frame>> {
    let tree = cdp
        .call(Some(session), "Page.getFrameTree", json!({}), deadline)
        .await?;
    let mut todo = VecDeque::from([&tree["frameTree"]]);
    let mut frames = Vec::new();
    while let Some(node) = todo.pop_front() {
        if frames.len() >= 32 {
            return Err(Failure::new("browser_frame_limit"));
        }
        let frame = &node["frame"];
        if let (Some(id), Some(loader)) = (frame["id"].as_str(), frame["loaderId"].as_str()) {
            frames.push(Frame {
                session: session.into(),
                id: id.into(),
                loader: loader.into(),
                url: frame["url"].as_str().unwrap_or("").into(),
                ancestors: Vec::new(),
            });
        }
        if let Some(children) = node["childFrames"].as_array() {
            todo.extend(children);
        }
    }
    Ok(frames)
}

async fn current(cdp: &Cdp, frame: &Frame, deadline: Instant) -> Result<()> {
    if !frame_tree(cdp, &frame.session, deadline)
        .await?
        .iter()
        .any(|f| f.id == frame.id && f.loader == frame.loader)
    {
        return Err(Failure::new("browser_stale_reference"));
    }
    Ok(())
}

pub async fn metadata(cdp: &Cdp, frame: &Frame, deadline: Instant) -> Result<Value> {
    let context = cdp
        .call(
            Some(&frame.session),
            "Page.createIsolatedWorld",
            json!({"frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}),
            deadline,
        )
        .await?;
    let result = cdp.call(Some(&frame.session), "Runtime.evaluate", json!({
        "contextId":context["executionContextId"],"returnByValue":true,
        "expression":"({url:location.href.slice(0,4096),title:document.title.slice(0,512)})",
    }),deadline).await?;
    current(cdp, frame, deadline).await?;
    let value = &result["result"]["value"];
    if !value["url"].is_string() || !value["title"].is_string() {
        return Err(Failure::new("browser_stale_reference"));
    }
    Ok(value.clone())
}

async fn object(cdp: &Cdp, frame: &Frame, backend: i64, deadline: Instant) -> Result<String> {
    let context = cdp
        .call(
            Some(&frame.session),
            "Page.createIsolatedWorld",
            json!({"frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}),
            deadline,
        )
        .await?;
    let result = cdp.call(Some(&frame.session), "DOM.resolveNode", json!({"backendNodeId":backend,"executionContextId":context["executionContextId"],"objectGroup":"rdh-observe"}), deadline).await?;
    result["object"]["objectId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Failure::new("browser_stale_reference"))
}

// Runs only in the isolated world. No caller-supplied code, selectors or method names.
const DETAILS: &str = r#"function(){
 if(!this.isConnected||this.nodeType!==1)return {connected:false};
 const text=e=>{const w=document.createTreeWalker(e,NodeFilter.SHOW_TEXT);let s='',n;for(let i=0;i<32&&s.length<512&&(n=w.nextNode());i++)s+=n.nodeValue.slice(0,512-s.length);return s;};
 const attr=(e,n)=>(e.getAttribute(n)||'').slice(0,512);
 const a=[];let p=this;for(let i=0;p&&i<4;i++,p=p.parentElement){a.push([p.tagName,attr(p,'id'),attr(p,'name'),attr(p,'role'),attr(p,'data-id'),attr(p,'aria-label'),text(p)]);}
 const r=this.getBoundingClientRect(),s=getComputedStyle(this),root=this.getRootNode(),hit=root.elementFromPoint(r.x+r.width/2,r.y+r.height/2);
 const select=this.tagName==='SELECT'?{multiple:this.multiple,complete:this.options.length<=32,options:Array.from({length:Math.min(32,this.options.length)},(_,i)=>{const o=this.options[i];return {value:o.value.length<=256?o.value:null,label:o.label.slice(0,256),label_truncated:o.label.length>256,selected:o.selected,disabled:o.disabled||!!o.closest('optgroup')?.disabled};})}:null;
 if(select)select.complete=select.complete&&select.options.every(o=>o.value!==null&&!o.label_truncated);
 if(select)a.push(select);
 return {connected:true,signature:JSON.stringify(a),tag:this.tagName,type:this.type||'',editable:(this.tagName==='INPUT'&&!['button','submit','checkbox','radio','file','hidden'].includes(this.type))||this.tagName==='TEXTAREA'||this.isContentEditable,
 enabled:!this.disabled&&!this.readOnly&&this.getAttribute('aria-disabled')!=='true',visible:r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none',hit:hit===this||this.contains(hit),x:r.x+r.width/2,y:r.y+r.height/2,checked:!!this.checked,select};}"#;

const SELECT: &str = r#"function(value){
 if(!this.isConnected||this.ownerDocument!==document||this.tagName!=='SELECT'||this.disabled||this.multiple||this.options.length>1024)return false;
 let chosen=null;for(const option of this.options){if(option.value===value){if(chosen||option.disabled||option.closest('optgroup')?.disabled)return false;chosen=option;}}
 if(!chosen)return false;
 Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(this,value);
 this.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
 this.dispatchEvent(new Event('change',{bubbles:true}));
 return true;
}"#;

async fn details(cdp: &Cdp, frame: &Frame, backend: i64, deadline: Instant) -> Result<Value> {
    let object = object(cdp, frame, backend, deadline).await?;
    let reply = cdp
        .call(
            Some(&frame.session),
            "Runtime.callFunctionOn",
            json!({"objectId":object,"functionDeclaration":DETAILS,"returnByValue":true}),
            deadline,
        )
        .await;
    let _ = cdp
        .call(
            Some(&frame.session),
            "Runtime.releaseObject",
            json!({"objectId":object}),
            deadline,
        )
        .await;
    let reply = reply?;
    if reply.get("exceptionDetails").is_some() {
        return Err(Failure::new("browser_stale_reference"));
    }
    Ok(reply["result"]["value"].clone())
}

fn interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "spinbutton"
            | "slider"
            | "switch"
            | "menuitem"
            | "tab"
            | "option"
    )
}

const COLLECT: &str = include_str!("observe.js");

pub async fn scroll(cdp: &Cdp, frame: &Frame, delta: &Value, deadline: Instant) -> Result<Value> {
    current(cdp, frame, deadline).await?;
    let context = cdp
        .call(
            Some(&frame.session),
            "Page.createIsolatedWorld",
            json!({
                "frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false,
            }),
            deadline,
        )
        .await?;
    let result = cdp.call_effect(Some(&frame.session), "Runtime.evaluate", json!({
        "contextId":context["executionContextId"],"returnByValue":true,
        "expression":format!("(()=>{{window.scrollBy(0,{delta});return {{scroll_x:window.scrollX,scroll_y:window.scrollY}};}})()"),
    }), deadline).await?;
    let mut value = result["result"]["value"].clone();
    if !value.is_object() {
        return Err(Failure::unknown("browser_not_actionable"));
    }
    value["input_acknowledged"] = true.into();
    value["business_result_verified"] = false.into();
    Ok(value)
}

pub async fn observe(
    cdp: &Cdp,
    frames: &[Frame],
    max_nodes: usize,
    max_chars: usize,
    deadline: Instant,
) -> Result<(Value, HashMap<String, Element>)> {
    let work_until = deadline.min(Instant::now() + Duration::from_secs(2));
    let observation = crate::random::hex_token::<12>().map_err(super::io_error)?;
    let mut nodes = Vec::new();
    let mut refs = HashMap::new();
    let mut remaining = max_chars;
    let mut visited = 0usize;
    let mut omitted = Vec::<String>::new();
    let mut observed = 0usize;
    for frame in frames {
        if nodes.len() >= max_nodes || remaining == 0 || visited >= 5000 {
            omitted.push("observation_budget".into());
            break;
        }
        current(cdp, frame, deadline).await?;
        let context = cdp.call(Some(&frame.session), "Page.createIsolatedWorld",
            json!({"frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}), deadline).await?;
        let collected = cdp.call(Some(&frame.session), "Runtime.evaluate", json!({
            "contextId":context["executionContextId"], "objectGroup":"rdh-observe",
            "expression":format!("({COLLECT})({},{})", (max_nodes-nodes.len()).min(200), 5000-visited),
        }), deadline).await?;
        let object = collected["result"]["objectId"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_stale_reference"))?;
        let result: Result<()> = async {
            let summary = cdp.call(Some(&frame.session), "Runtime.callFunctionOn", json!({
                "objectId":object,"functionDeclaration":"function(){return {nodes:this.summary,scanned:this.scanned,omitted:this.omitted};}","returnByValue":true,
            }), deadline).await?;
            let summary = &summary["result"]["value"];
            visited += summary["scanned"].as_u64().unwrap_or(0) as usize;
            if let Some(values) = summary["omitted"].as_array() {
                omitted.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            let candidates = summary["nodes"].as_array()
                .ok_or_else(|| Failure::new("browser_stale_reference"))?;
            for candidate in candidates {
                if remaining == 0 || nodes.len() >= max_nodes || Instant::now() + Duration::from_millis(200) >= work_until {
                    omitted.push("observation_budget".into()); break;
                }
                let mut backend = None;
                let ax;
                let node = if let Some(index) = candidate["index"].as_u64() {
                    let remote = cdp.call(Some(&frame.session), "Runtime.callFunctionOn", json!({
                        "objectId":object,"functionDeclaration":"function(i){return this.nodes[i];}",
                        "arguments":[{"value":index}],"objectGroup":"rdh-observe",
                    }), deadline).await?;
                    let dom = cdp.call(Some(&frame.session), "DOM.describeNode", json!({
                        "objectId":remote["result"]["objectId"],"depth":0,
                    }), deadline).await?;
                    backend = dom["node"]["backendNodeId"].as_i64();
                    ax = cdp.call(Some(&frame.session), "Accessibility.getPartialAXTree", json!({
                        "backendNodeId":backend,"fetchRelatives":false,
                    }), deadline).await?;
                    let Some(node) = ax["nodes"].as_array().and_then(|v| v.iter().find(|n| n["backendDOMNodeId"].as_i64() == backend && n["ignored"] != true)) else { continue; };
                    node
                } else { candidate };
                let role = if backend.is_some() { node["role"]["value"].as_str() } else { node["role"].as_str() }.unwrap_or("");
                let full_name = if backend.is_some() { node["name"]["value"].as_str() } else { node["name"].as_str() }.unwrap_or("");
                let name = clipped(full_name, 512.min(remaining));
                if name.len() < full_name.len() { omitted.push("text_clipped".into()); }
                if name.is_empty() && !interactive(role) { continue; }
                remaining = remaining.saturating_sub(name.chars().count() + role.len());
                let mut visible = json!({"role":role,"name":name,"frame_id":frame.id});
                if interactive(role) && let Some(backend) = backend {
                    let data = details(cdp, frame, backend, deadline).await?;
                    if data["connected"] == true {
                        let mut choices = Vec::new();
                        if let Some(options) = data["select"]["options"].as_array() {
                            let mut shown = Vec::new();
                            for option in options {
                                let size = ["value","label"].iter().filter_map(|k|option[*k].as_str()).map(|v|v.chars().count()).sum::<usize>() + 32;
                                if size > remaining { break; }
                                remaining -= size;
                                if option["disabled"] != true && let Some(value) = option["value"].as_str() { choices.push(value.into()); }
                                shown.push(option.clone());
                            }
                            let complete = data["select"]["complete"] == true && shown.len() == options.len();
                            visible["options"] = shown.into();
                            visible["options_complete"] = complete.into();
                            visible["multiple"] = data["select"]["multiple"].clone();
                            if !complete { omitted.push("select_options_omitted".into()); }
                        }
                        let reference = format!("e_{observation}_{}", refs.len());
                        visible["ref"] = reference.clone().into();
                        visible["enabled"] = data["enabled"].clone();
                        visible["editable"] = data["editable"].clone();
                        visible["sensitive"] = (data["type"] == "password").into();
                        if data["type"] != "password" && let Some(value) = node["value"]["value"].as_str() {
                            let shown = clipped(value, remaining.min(512));
                            remaining = remaining.saturating_sub(shown.chars().count());
                            if shown.len() < value.len() { omitted.push("value_clipped".into()); }
                            visible["value"] = shown.into();
                        }
                        if matches!(data["type"].as_str(), Some("checkbox" | "radio")) {
                            visible["checked"] = data["checked"].clone();
                        }
                        refs.insert(reference, Element {
                            frame: frame.clone(), backend,
                            signature: crate::config::sha256_hex(data["signature"].as_str().unwrap_or("").as_bytes()),
                            role: role.into(), name: crate::config::sha256_hex(full_name.as_bytes()), choices,
                        });
                    }
                }
                nodes.push(visible);
            }
            current(cdp, frame, deadline).await?;
            Ok(())
        }.await;
        let _ = cdp
            .call(
                Some(&frame.session),
                "Runtime.releaseObjectGroup",
                json!({"objectGroup":"rdh-observe"}),
                deadline,
            )
            .await;
        result?;
        observed += 1;
    }
    if observed < frames.len() {
        omitted.push("frames_omitted".into());
    }
    omitted.sort();
    omitted.dedup();
    Ok((
        json!({"observation_id":observation,"nodes":nodes,"coverage":{
            "scope":"document_text_and_controls_open_shadow","complete_within_scope":omitted.is_empty(),
            "omitted":omitted,"frames_observed":observed,"frames_omitted":frames.len()-observed,"nodes_scanned":visited,
        }}),
        refs,
    ))
}

impl Element {
    async fn ancestor_hit(
        &self,
        cdp: &Cdp,
        mut x: f64,
        mut y: f64,
        deadline: Instant,
    ) -> Result<()> {
        let mut child_session = self.frame.session.as_str();
        for (parent_session, child_frame) in &self.frame.ancestors {
            let metrics = cdp
                .call(
                    Some(child_session),
                    "Page.getLayoutMetrics",
                    json!({}),
                    deadline,
                )
                .await?;
            let viewport = &metrics["cssLayoutViewport"];
            let width = viewport["clientWidth"].as_f64().unwrap_or(0.);
            let height = viewport["clientHeight"].as_f64().unwrap_or(0.);
            if width <= 0. || height <= 0. || x < 0. || y < 0. || x >= width || y >= height {
                return Err(Failure::new("browser_not_actionable"));
            }
            let owner = cdp
                .call(
                    Some(parent_session),
                    "DOM.getFrameOwner",
                    json!({"frameId":child_frame}),
                    deadline,
                )
                .await?;
            let quads = cdp
                .call(
                    Some(parent_session),
                    "DOM.getContentQuads",
                    json!({"backendNodeId":owner["backendNodeId"]}),
                    deadline,
                )
                .await?;
            let q = quads["quads"]
                .as_array()
                .and_then(|v| v.first())
                .and_then(Value::as_array)
                .filter(|v| v.len() == 8)
                .and_then(|v| v.iter().map(Value::as_f64).collect::<Option<Vec<_>>>())
                .ok_or_else(|| Failure::new("browser_not_actionable"))?;
            let (u, v) = (x / width, y / height);
            x = q[0] * (1. - u) * (1. - v)
                + q[2] * u * (1. - v)
                + q[4] * u * v
                + q[6] * (1. - u) * v;
            y = q[1] * (1. - u) * (1. - v)
                + q[3] * u * (1. - v)
                + q[5] * u * v
                + q[7] * (1. - u) * v;
            let hit = cdp
                .call(
                    Some(parent_session),
                    "DOM.getNodeForLocation",
                    json!({"x":x.round() as i64,"y":y.round() as i64}),
                    deadline,
                )
                .await?;
            if hit["frameId"] != *child_frame && hit["backendNodeId"] != owner["backendNodeId"] {
                return Err(Failure::new("browser_not_actionable"));
            }
            child_session = parent_session;
        }
        Ok(())
    }
    async fn point(&self, cdp: &Cdp, deadline: Instant) -> Result<Value> {
        // Browser input uses the target's main-frame viewport. A DOM rect from a
        // same-process subframe is in that frame's coordinates, so never send it.
        let quads = cdp
            .call(
                Some(&self.frame.session),
                "DOM.getContentQuads",
                json!({"backendNodeId":self.backend}),
                deadline,
            )
            .await?;
        let quad = quads["quads"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(Value::as_array)
            .filter(|v| v.len() == 8)
            .ok_or_else(|| Failure::new("browser_not_actionable"))?;
        let points = quad
            .iter()
            .map(Value::as_f64)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Failure::new("browser_not_actionable"))?;
        let x = (points[0] + points[2] + points[4] + points[6]) / 4.;
        let y = (points[1] + points[3] + points[5] + points[7]) / 4.;
        if !x.is_finite() || !y.is_finite() || x < 0. || y < 0. {
            return Err(Failure::new("browser_not_actionable"));
        }
        let hit = cdp
            .call(
                Some(&self.frame.session),
                "DOM.getNodeForLocation",
                json!({"x":x.round() as i64,"y":y.round() as i64}),
                deadline,
            )
            .await?;
        if hit["frameId"] != self.frame.id {
            return Err(Failure::new("browser_not_actionable"));
        }
        self.ancestor_hit(cdp, x, y, deadline).await?;
        Ok(json!({"x":x,"y":y}))
    }
    async fn checked(&self, cdp: &Cdp, deadline: Instant) -> Result<Value> {
        current(cdp, &self.frame, deadline).await?;
        let data = details(cdp, &self.frame, self.backend, deadline).await?;
        if data["connected"] != true
            || crate::config::sha256_hex(data["signature"].as_str().unwrap_or("").as_bytes())
                != self.signature
        {
            return Err(Failure::new("browser_stale_reference"));
        }
        let ax = cdp
            .call(
                Some(&self.frame.session),
                "Accessibility.getPartialAXTree",
                json!({"backendNodeId":self.backend,"fetchRelatives":false}),
                deadline,
            )
            .await?;
        if !ax["nodes"].as_array().is_some_and(|nodes| {
            nodes.iter().any(|n| {
                n["backendDOMNodeId"] == self.backend
                    && n["role"]["value"] == self.role
                    && crate::config::sha256_hex(
                        n["name"]["value"].as_str().unwrap_or("").as_bytes(),
                    ) == self.name
            })
        }) {
            return Err(Failure::new("browser_stale_reference"));
        }
        if data["enabled"] != true || data["visible"] != true {
            return Err(Failure::new("browser_not_actionable"));
        }
        Ok(data)
    }

    pub async fn act(&self, cdp: &Cdp, action: &Value, deadline: Instant) -> Result<Value> {
        let kind = action["kind"].as_str().unwrap_or("");
        self.checked(cdp, deadline).await?;
        cdp.call(
            Some(&self.frame.session),
            "DOM.scrollIntoViewIfNeeded",
            json!({"backendNodeId":self.backend}),
            deadline,
        )
        .await?;
        let first = self.point(cdp, deadline).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let data = self.checked(cdp, deadline).await?;
        let point = self.point(cdp, deadline).await?;
        if data["hit"] != true || first["x"] != point["x"] || first["y"] != point["y"] {
            return Err(Failure::new("browser_not_actionable"));
        }
        let session = Some(self.frame.session.as_str());
        match kind {
            "select" => {
                let value = action["value"].as_str().unwrap_or("");
                if data["tag"] != "SELECT"
                    || data["select"]["multiple"] == true
                    || !self.choices.iter().any(|v| v == value)
                {
                    return Err(Failure::new("browser_invalid_params"));
                }
                let object = object(cdp, &self.frame, self.backend, deadline).await?;
                let result = cdp.call_effect(session,"Runtime.callFunctionOn",json!({"objectId":object,
                    "functionDeclaration":SELECT,"arguments":[{"value":value}],"returnByValue":true}),deadline).await;
                let _ = cdp
                    .cleanup()
                    .call(
                        session,
                        "Runtime.releaseObject",
                        json!({"objectId":object}),
                        deadline,
                    )
                    .await;
                if result?["result"]["value"] != true {
                    return Err(Failure::unknown("browser_not_actionable"));
                }
            }
            "click" | "check" => {
                if kind == "check" {
                    if !matches!(data["type"].as_str(), Some("checkbox" | "radio")) {
                        return Err(Failure::new("browser_invalid_params"));
                    }
                    if data["checked"] == action["checked"] {
                        return Ok(json!({"changed":false}));
                    }
                }
                cdp.input_pair(session,"Input.dispatchMouseEvent",json!({"type":"mousePressed","x":point["x"],"y":point["y"],"button":"left","clickCount":1}),json!({"type":"mouseReleased","x":point["x"],"y":point["y"],"button":"left","clickCount":1}),deadline).await?;
            }
            "fill" => {
                if data["editable"] != true || data["type"] == "password" {
                    return Err(Failure::new("browser_sensitive_input"));
                }
                cdp.call(
                    session,
                    "DOM.focus",
                    json!({"backendNodeId":self.backend}),
                    deadline,
                )
                .await?;
                // Input.insertText follows the browser's editing path, including frontend input events.
                cdp.input_pair(session,"Input.dispatchKeyEvent",json!({"type":"keyDown","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65}),json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65}),deadline).await?;
                cdp.call(
                    session,
                    "Input.insertText",
                    json!({"text":action["text"]}),
                    deadline,
                )
                .await?;
            }
            "press" => {
                cdp.call(
                    session,
                    "DOM.focus",
                    json!({"backendNodeId":self.backend}),
                    deadline,
                )
                .await?;
                let key = action["key"].as_str().unwrap_or("");
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
                cdp.input_pair(session,"Input.dispatchKeyEvent",json!({"type":"keyDown","key":key,"windowsVirtualKeyCode":code,"text":if key=="Enter"{"\r"}else{""}}),json!({"type":"keyUp","key":key,"windowsVirtualKeyCode":code}),deadline).await?;
            }
            _ => return Err(Failure::new("browser_invalid_params")),
        }
        Ok(json!({"input_acknowledged":true,"business_result_verified":false}))
    }
}
