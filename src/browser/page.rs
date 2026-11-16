//! Bounded, document-bound semantic observations and typed element actions.
use super::{Failure, Result, cdp::Cdp};
use crate::browser::cdp::{Command, InputPair};
use crate::browser::control::Effect;
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
    /// The `#...` part of the address, which the frame tree reports apart from `url`.
    pub fragment: String,
    pub ancestors: Vec<(String, String)>,
}
pub struct Element {
    frame: Frame,
    backend: i64,
    signature: String,
    role: String,
    name: String,
    choices: Vec<String>,
    /// The element only looks like a control. The page vouches for it and names
    /// it, and it takes clicks and keys only.
    inferred: bool,
}

/// Why an element has a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A control that HTML defines.
    Native,
    /// An element whose role attribute names a control.
    Aria,
    /// An element that only looks like a control: its markup holds a click
    /// handler, the pointer turns into a hand over it, or it is in the focus
    /// order.
    Inferred,
}

impl Kind {
    /// The kind that the page reader gave a candidate, if it gave one.
    fn of(candidate: &Value) -> Option<Self> {
        match candidate["kind"].as_str()? {
            "native" => Some(Self::Native),
            "aria" => Some(Self::Aria),
            "inferred" => Some(Self::Inferred),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Aria => "aria",
            Self::Inferred => "inferred",
        }
    }
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
        .call(
            Some(session),
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
        if let (Some(id), Some(loader)) = (frame["id"].as_str(), frame["loaderId"].as_str()) {
            frames.push(Frame {
                session: session.into(),
                id: id.into(),
                loader: loader.into(),
                url: frame["url"].as_str().unwrap_or("").into(),
                fragment: frame["urlFragment"].as_str().unwrap_or("").into(),
                ancestors: Vec::new(),
            });
        }
        if let Some(children) = node["childFrames"].as_array() {
            todo.extend(children);
        }
    }
    Ok(frames)
}

/// Confirms the frame still holds the observed document. Returns whether it is
/// the root frame of its target.
async fn current(cdp: &Cdp, frame: &Frame, deadline: Instant) -> Result<bool> {
    let frames = frame_tree(cdp, &frame.session, deadline).await?;
    if !frames
        .iter()
        .any(|f| f.id == frame.id && f.loader == frame.loader)
    {
        return Err(Failure::new("browser_stale_reference"));
    }
    Ok(frames.first().is_some_and(|f| f.id == frame.id))
}

pub async fn metadata(cdp: &Cdp, frame: &Frame, deadline: Instant) -> Result<Value> {
    let context = cdp
        .call(
            Some(&frame.session),
            Command::PageCreateIsolatedWorld,
            json!({"frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}),
            deadline,
        )
        .await?;
    let result = cdp.call(Some(&frame.session), Command::RuntimeEvaluate(Effect::None), json!({
        "contextId":context["executionContextId"],"returnByValue":true,
        "expression":"({url:location.href.slice(0,4096),title:document.title.slice(0,512),ready_state:document.readyState})",
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
            Command::PageCreateIsolatedWorld,
            json!({"frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}),
            deadline,
        )
        .await?;
    let result = cdp.call(Some(&frame.session), Command::DomResolveNode, json!({"backendNodeId":backend,"executionContextId":context["executionContextId"],"objectGroup":"rdh-observe"}), deadline).await?;
    result["object"]["objectId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Failure::new("browser_stale_reference"))
}

/// What marks an element as a control on the page itself. The reader and the
/// details of an element decide it with the same code.
const MARKS: &str = include_str!("marks.js");

// Runs only in the isolated world. No caller-supplied code, selectors or method names.
const DETAILS: &str = concat!(
    "function(){\n const marks=",
    include_str!("marks.js"),
    ";\n",
    r#" if(!this.isConnected||this.nodeType!==1)return {connected:false};
 const text=e=>{const w=document.createTreeWalker(e,NodeFilter.SHOW_TEXT);let s='',n;for(let i=0;i<32&&s.length<512&&(n=w.nextNode());i++)s+=n.nodeValue.slice(0,512-s.length);return s;};
 const attr=(e,n)=>(e.getAttribute(n)||'').slice(0,512);
 // What a reference means: the element with its own text, the tags and naming attributes of
 // three ancestors, and the text of the record it belongs to where the markup marks one. Other
 // text nearby, such as a clock or a progress value beside a button, is not part of it.
 const a=[];let p=this;for(let i=0;p&&i<4;i++,p=p.parentElement){a.push([p.tagName,attr(p,'id'),attr(p,'name'),attr(p,'role'),attr(p,'data-id'),attr(p,'aria-label'),i===0?text(p):'']);}
 const record=marks.record;
 let owner=null;
 for(let q=this.parentElement||this.getRootNode().host,i=0;q&&i<8;i++,q=q.parentElement||q.getRootNode().host){if(record(q)){owner=q;a.push(['record',text(q)]);break;}}
 // While a reader is at work, whether the record still shows what the reader reported of it.
 const reader=globalThis.__rdhBrowserReader,steady=!owner||!reader||reader.closed||reader.steady(owner);
 const r=this.getBoundingClientRect(),s=getComputedStyle(this),root=this.getRootNode();
 // Input is sent to the first fragment, so the hit test looks there too.
 const f=this.getClientRects()[0]||r,x=f.x+f.width/2,y=f.y+f.height/2,top=root.elementFromPoint(x,y),hit=top===this||this.contains(top);
 // Scrolling into view does nothing when the box is already fully visible in
 // the viewport and in every scrolling ancestor.
 const html=document.documentElement,clips=e=>{const o=getComputedStyle(e);return o.overflowX!=='visible'||o.overflowY!=='visible';};
 let inView=r.left>=0&&r.top>=0&&r.right<=innerWidth&&r.bottom<=innerHeight,up=this,depth=0;
 while(inView&&(up=up.assignedSlot||up.parentElement||up.getRootNode().host)){
  if(++depth>64){inView=false;break;}
  // The viewport test covers the root, and a body whose overflow the viewport took over.
  if(up===html||(up===document.body&&!clips(html))||!clips(up))continue;
  const c=up.getBoundingClientRect(),l=c.left+up.clientLeft,t=c.top+up.clientTop;
  inView=r.left>=l&&r.top>=t&&r.right<=l+up.clientWidth&&r.bottom<=t+up.clientHeight;
 }
 // Painted at the click point without being the top element there: something covers it.
 const covered=!hit&&root.elementsFromPoint(x,y).some(e=>e===this||this.contains(e));
 // A click at that point would be taken by another control inside this element.
 let inner=false;for(let q=top;hit&&q&&q!==this;q=q.parentElement||q.getRootNode().host){if(marks.marked(q)||q.tagName==='LABEL'){inner=true;break;}}
 // An element that only looks like a control is named by the page: by the text it shows
 // outside the controls it holds, or else by a label in its markup.
 const words=()=>{const w=document.createTreeWalker(this,NodeFilter.SHOW_ELEMENT|NodeFilter.SHOW_TEXT,{acceptNode:n=>n.nodeType===1&&marks.marked(n)?NodeFilter.FILTER_REJECT:NodeFilter.FILTER_ACCEPT});let s='',n;
  for(let i=0;i<128&&s.length<=80&&(n=w.nextNode());i++){if(n.nodeType!==3)continue;const t=n.nodeValue.replace(/\s+/g,' ').trim(),p=n.parentElement;if(t&&p&&p.getClientRects().length&&getComputedStyle(p).visibility!=='hidden')s+=(s?' ':'')+t;}
  if(!s){const q=this.querySelector('img[alt],title');s=(attr(this,'aria-label')||attr(this,'title')||attr(this,'alt')||(q&&(q.getAttribute('alt')||q.textContent))||'').replace(/\s+/g,' ').trim();}
  return Array.from(s).slice(0,80).join('');};
 const readonly=(this.tagName==='INPUT'||this.tagName==='TEXTAREA')&&this.readOnly;
 const select=this.tagName==='SELECT'?{multiple:this.multiple,complete:this.options.length<=32,options:Array.from({length:Math.min(32,this.options.length)},(_,i)=>{const o=this.options[i];return {value:o.value.length<=256?o.value:null,label:o.label.slice(0,256),label_truncated:o.label.length>256,selected:o.selected,disabled:o.disabled||!!o.closest('optgroup')?.disabled};})}:null;
 if(select)select.complete=select.complete&&select.options.every(o=>o.value!==null&&!o.label_truncated);
 if(select)a.push(select);
 return {connected:true,signature:JSON.stringify(a),tag:this.tagName,type:this.type||'',editable:!readonly&&((this.tagName==='INPUT'&&!['button','submit','checkbox','radio','file','hidden'].includes(this.type))||this.tagName==='TEXTAREA'||this.isContentEditable),readonly,
 enabled:!this.disabled&&this.getAttribute('aria-disabled')!=='true',visible:r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none',hit,in_view:inView,covered,inner,x,y,checked:!!this.checked,select,steady,
 words:marks.native(this)||marks.role(this)?'':words()};}"#
);

const SELECT: &str = r#"function(value){
 if(!this.isConnected||this.ownerDocument!==document||this.tagName!=='SELECT'||this.disabled||this.multiple||this.options.length>1024)return false;
 let chosen=null;for(const option of this.options){if(option.value===value){if(chosen||option.disabled||option.closest('optgroup')?.disabled)return false;chosen=option;}}
 if(!chosen)return false;
 Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(this,value);
 this.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
 this.dispatchEvent(new Event('change',{bubbles:true}));
 return true;
}"#;

// A date, a time, a colour and a slider hold a value that is picked, not typed.
// It is set the way a picker sets it. The field keeps only a value that it can
// hold, and a field of the same type and range that is not part of the page
// shows what that is. Without `apply` the function only says whether the value
// fits, and leaves the page as it was.
const SET: &str = r#"function(value,apply){
 if(!this.isConnected||this.ownerDocument!==document||this.tagName!=='INPUT'||this.disabled)return false;
 const probe=document.createElement('input');probe.type=this.type;
 for(const name of ['min','max','step'])if(this.hasAttribute(name))probe.setAttribute(name,this.getAttribute(name));
 probe.value=value;
 // The browser writes a colour in lower case, and a number in its shortest form.
 const fits=this.type==='range'?value.trim()!==''&&Number(probe.value)===Number(value):probe.value.toLowerCase()===value.toLowerCase();
 if(!fits||!apply)return fits;
 const before=this.value;
 Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(this,value);
 if(this.value!==before){
  this.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
  this.dispatchEvent(new Event('change',{bubbles:true}));
 }
 return true;
}"#;

/// Whether the element that `data` describes holds a value that is picked: a
/// date, a time, a colour or a slider. Typed text does nothing to such a field.
fn picked(data: &Value) -> bool {
    data["tag"] == "INPUT"
        && matches!(
            data["type"].as_str(),
            Some("date" | "time" | "datetime-local" | "month" | "week" | "color" | "range")
        )
}

async fn details(cdp: &Cdp, frame: &Frame, backend: i64, deadline: Instant) -> Result<Value> {
    let object = object(cdp, frame, backend, deadline).await?;
    let reply = cdp
        .call(
            Some(&frame.session),
            Command::RuntimeCallFunction(Effect::None),
            json!({"objectId":object,"functionDeclaration":DETAILS,"returnByValue":true}),
            deadline,
        )
        .await;
    let _ = cdp
        .call(
            Some(&frame.session),
            Command::RuntimeReleaseObject,
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

/// The roles by which the browser itself calls an element a control. It knows
/// of click handlers that the markup does not show, so what it calls a control
/// is one even where the page does not mark it.
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
            Command::PageCreateIsolatedWorld,
            json!({
                "frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false,
            }),
            deadline,
        )
        .await?;
    let result = cdp.call(Some(&frame.session), Command::RuntimeEvaluate(Effect::Page), json!({
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

/// Which part of a page a read is asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Text and controls, in the order of the document.
    #[default]
    All,
    /// Controls and headings, without the text between them.
    Interactive,
    /// What lies in the part of the page that shows now.
    Viewport,
}

/// What a read is asked for. A sequence keeps it from its first call to its
/// last.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub filter: Filter,
    /// The one frame to read, by its id. Without it every frame is read.
    pub within: Option<String>,
    /// The text to look for. What is found is returned with its record.
    pub find: Option<String>,
}

/// How many places one search returns.
const PLACES: usize = 20;

impl Scope {
    /// What the page reader of one frame is told. `limit` is the number of
    /// places it may still find.
    fn options(&self, limit: usize) -> Value {
        json!({
            "interactive":self.filter == Filter::Interactive,
            "viewport":self.filter == Filter::Viewport,
            "find":self.find,"limit":limit,
        })
    }
    /// How many nodes of a page one call looks at. A read that leaves most of
    /// the page out passes over it without asking the browser anything, so it
    /// looks further.
    fn reach(&self) -> usize {
        if self.filter == Filter::All && self.find.is_none() {
            5000
        } else {
            50_000
        }
    }
}

pub struct Cursor {
    scope: Scope,
    /// The tab's own document, whether or not it is among the frames to read.
    top: Frame,
    frames: Vec<Frame>,
    readers: Vec<Option<String>>,
    /// The places that the reader of each frame has found.
    places: Vec<usize>,
    frame: usize,
    pages: usize,
    scanned: usize,
    omitted: Vec<String>,
    /// The page changed something that the sequence had already read.
    changed: bool,
}

fn same_document(a: &Frame, b: &Frame) -> bool {
    a.session == b.session && a.id == b.id && a.loader == b.loader && a.url == b.url
}

/// A failure that means the thing being read is no longer there, as opposed to
/// the browser being lost or a deadline passing.
fn gone(error: &Failure) -> bool {
    matches!(
        error.code,
        "browser_stale_reference" | "browser_command_failed"
    )
}

impl Cursor {
    pub fn new(frames: &[Frame], scope: Scope) -> Result<Self> {
        if frames.is_empty() || frames.len() > 32 {
            return Err(Failure::new("browser_frame_limit"));
        }
        let top = frames[0].clone();
        let frames: Vec<Frame> = match &scope.within {
            Some(id) => frames.iter().filter(|f| f.id == *id).cloned().collect(),
            None => frames.to_vec(),
        };
        // The frame that was asked for is not in the page, or is there no longer.
        if frames.is_empty() {
            return Err(Failure::new("browser_stale_reference"));
        }
        Ok(Self {
            scope,
            top,
            readers: vec![None; frames.len()],
            places: vec![0; frames.len()],
            frames,
            frame: 0,
            pages: 0,
            scanned: 0,
            omitted: Vec::new(),
            changed: false,
        })
    }
    /// A sequence can continue while the tab shows the document it began on.
    pub fn matches(&self, frames: &[Frame]) -> bool {
        frames
            .first()
            .is_some_and(|now| same_document(&self.top, now))
    }
    /// Notes subframes that came, went or navigated since the sequence began.
    /// It reads the ones it began with for as long as they last, and no others.
    /// A sequence that reads one frame looks at that frame only.
    pub fn compare(&mut self, frames: &[Frame]) {
        let same = match &self.scope.within {
            Some(_) => self
                .frames
                .iter()
                .all(|mine| frames.iter().any(|now| same_document(mine, now))),
            None => {
                self.frames.len() == frames.len()
                    && self
                        .frames
                        .iter()
                        .zip(frames)
                        .all(|(a, b)| same_document(a, b))
            }
        };
        if !same {
            self.omit("frame_changed");
        }
    }
    pub fn more(&self) -> bool {
        self.frame < self.frames.len()
    }
    fn omit(&mut self, reason: &str) {
        if !self.omitted.iter().any(|known| known == reason) {
            self.omitted.push(reason.into());
        }
    }
    /// Asks each reader whether it is still the current one and whether the page
    /// changed what it had read. A reader that a later observation replaced ends
    /// the sequence, and so does losing the reader of the tab's own document. A
    /// subframe's reader goes with its document, and takes only that frame.
    async fn check(&mut self, cdp: &Cdp, deadline: Instant) -> Result<()> {
        for index in 0..self.frames.len() {
            let Some(reader) = &self.readers[index] else {
                continue;
            };
            let status=cdp.call(Some(&self.frames[index].session),Command::RuntimeCallFunction(Effect::None),json!({"objectId":reader,"functionDeclaration":"function(){return this.status();}","returnByValue":true}),deadline).await;
            match status {
                Ok(value) if value["result"]["value"]["closed"] == false => {
                    self.changed |= value["result"]["value"]["changed"] == true;
                }
                Ok(_) => return Err(Failure::new("browser_stale_reference")),
                Err(error) if index > 0 && gone(&error) => {
                    self.readers[index] = None;
                    self.omit("frame_changed");
                    if index == self.frame {
                        self.frame += 1;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    pub async fn close(&self, cdp: &Cdp, deadline: Instant) {
        for (frame, reader) in self.frames.iter().zip(&self.readers) {
            if let Some(reader) = reader {
                let _=cdp.cleanup().call(Some(&frame.session),Command::RuntimeCallFunction(Effect::None),json!({"objectId":reader,"functionDeclaration":"function(){this.close();}"}),deadline).await;
                let _ = cdp
                    .cleanup()
                    .call(
                        Some(&frame.session),
                        Command::RuntimeReleaseObjectGroup,
                        json!({"objectGroup":format!("rdh-reader-{}",frame.id)}),
                        deadline,
                    )
                    .await;
            }
        }
    }
}

pub async fn observe(
    cdp: &Cdp,
    frames: &[Frame],
    max_nodes: usize,
    max_chars: usize,
    deadline: Instant,
) -> Result<(Value, HashMap<String, Element>)> {
    let mut cursor = Cursor::new(frames, Scope::default())?;
    let result = observe_next(cdp, &mut cursor, max_nodes, max_chars, deadline).await;
    cursor.close(cdp, deadline).await;
    result
}

/// What the browser reports about one element the reader found.
struct Found {
    backend: i64,
    /// Whether the accessibility tree has anything to say about the element.
    exposed: bool,
    role: String,
    name: String,
    value: Value,
    /// What a reference is bound to. Only controls have it.
    data: Option<Value>,
}

/// How many elements are described together.
const TOGETHER: usize = 16;

/// Finds what the browser calls the element at `index` of the reader's current
/// batch. `None` means the browser no longer knows the element.
async fn located(
    cdp: &Cdp,
    frame: &Frame,
    reader: &str,
    index: u64,
    deadline: Instant,
) -> Result<Option<Found>> {
    let session = Some(frame.session.as_str());
    let remote = cdp
        .call(
            session,
            Command::RuntimeCallFunction(Effect::None),
            json!({
                "objectId":reader,"functionDeclaration":"function(i){return this.pending[i].node;}",
                "arguments":[{"value":index}],"objectGroup":"rdh-observe",
            }),
            deadline,
        )
        .await?;
    let dom = cdp
        .call(
            session,
            Command::DomDescribeNode,
            json!({"objectId":remote["result"]["objectId"],"depth":0}),
            deadline,
        )
        .await?;
    let Some(backend) = dom["node"]["backendNodeId"].as_i64() else {
        return Ok(None);
    };
    let ax = cdp
        .call(
            session,
            Command::AccessibilityGetPartialAXTree,
            json!({"backendNodeId":backend,"fetchRelatives":false}),
            deadline,
        )
        .await?;
    let node = ax["nodes"].as_array().and_then(|nodes| {
        nodes
            .iter()
            .find(|n| n["backendDOMNodeId"].as_i64() == Some(backend) && n["ignored"] != true)
    });
    let text = |field: &str| {
        node.and_then(|n| n[field]["value"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    Ok(Some(Found {
        backend,
        exposed: node.is_some(),
        role: text("role"),
        name: text("name"),
        value: node.map_or(Value::Null, |n| n["value"]["value"].clone()),
        data: None,
    }))
}

/// Asks the page, in one call, for the details of the elements at `indexes` of
/// the reader's current batch. The reader holds them in the world where the
/// details are read.
async fn details_of(
    cdp: &Cdp,
    frame: &Frame,
    reader: &str,
    indexes: &[u64],
    deadline: Instant,
) -> Result<Value> {
    if indexes.is_empty() {
        return Ok(Value::Null);
    }
    cdp.call(
        Some(&frame.session),
        Command::RuntimeCallFunction(Effect::None),
        json!({
            "objectId":reader,
            "functionDeclaration":format!("function(indexes){{const details={DETAILS};return indexes.map(i=>{{try{{return details.call(this.pending[i].node);}}catch(error){{return null;}}}});}}"),
            "arguments":[{"value":indexes}],"returnByValue":true,
        }),
        deadline,
    )
    .await
}

/// Adds the details that the page gave for the elements at `indexes`. An
/// element that left the page after the reader found it is reported as a stale
/// reference.
fn detailed(
    described: &mut HashMap<u64, Result<Option<Found>>>,
    indexes: &[u64],
    details: Result<Value>,
) {
    for (position, index) in indexes.iter().enumerate() {
        let data = match &details {
            Ok(reply) => &reply["result"]["value"][position],
            Err(error) => {
                let error = Failure {
                    code: error.code,
                    state: error.state,
                };
                described.insert(*index, Err(error));
                continue;
            }
        };
        match described.get_mut(index) {
            Some(Ok(Some(found))) if data["connected"] == true => found.data = Some(data.clone()),
            Some(Ok(_)) => {
                described.insert(*index, Err(Failure::new("browser_stale_reference")));
            }
            // What went wrong while asking the browser stands.
            _ => {}
        }
    }
}

/// Describes several elements of the reader's current batch, by their indexes.
/// `controls` are the ones among them that the page marks as controls. The
/// browser is asked about all of the elements at once, and at the same time
/// the page is asked for the details of the controls in one call: on a slow or
/// busy device every call that is waited for alone costs a turn of waiting.
///
/// Each element has its own outcome. `None` means the browser no longer knows
/// it. An element that left the page after the reader found it is reported as
/// a stale reference.
async fn found(
    cdp: &Cdp,
    frame: &Frame,
    reader: &str,
    indexes: &[u64],
    controls: &[u64],
    deadline: Instant,
) -> HashMap<u64, Result<Option<Found>>> {
    let (located, details) = tokio::join!(
        futures_util::future::join_all(
            indexes
                .iter()
                .map(|index| located(cdp, frame, reader, *index, deadline)),
        ),
        details_of(cdp, frame, reader, controls, deadline),
    );
    let mut described: HashMap<u64, Result<Option<Found>>> =
        indexes.iter().copied().zip(located).collect();
    detailed(&mut described, controls, details);
    // What the browser alone calls a control has a reference as well.
    let more: Vec<u64> = indexes
        .iter()
        .copied()
        .filter(|index| {
            !controls.contains(index)
                && matches!(&described[index], Ok(Some(found)) if found.exposed && interactive(&found.role))
        })
        .collect();
    let details = details_of(cdp, frame, reader, &more, deadline).await;
    detailed(&mut described, &more, details);
    described
}

/// Reads the next segment. The page may change while it is read: the read goes
/// on, and the coverage says so. It fails only when the tab's own document is
/// replaced or a later observation takes the reader's place.
pub async fn observe_next(
    cdp: &Cdp,
    cursor: &mut Cursor,
    max_nodes: usize,
    max_chars: usize,
    deadline: Instant,
) -> Result<(Value, HashMap<String, Element>)> {
    let work_until = deadline.min(Instant::now() + Duration::from_secs(2));
    cursor.check(cdp, deadline).await?;
    let (filter, reach) = (cursor.scope.filter, cursor.scope.reach());
    let observation = crate::random::hex_token::<12>().map_err(super::io_error)?;
    let mut nodes = Vec::new();
    let mut refs = HashMap::new();
    let mut remaining = max_chars;
    let mut visited = 0usize;
    let mut omitted = Vec::<String>::new();
    let mut observed = std::collections::HashSet::new();
    let mut changed = false;
    while cursor.more() {
        if nodes.len() >= max_nodes
            || remaining == 0
            || visited >= reach
            || Instant::now() + Duration::from_millis(200) >= work_until
        {
            break;
        }
        let index = cursor.frame;
        let frame = cursor.frames[index].clone();
        let result:Result<bool>=async {
            current(cdp, &frame, deadline).await?;
            if cursor.readers[index].is_none() {
                let context=cdp.call(Some(&frame.session),Command::PageCreateIsolatedWorld,json!({"frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false}),deadline).await?;
                let group = format!("rdh-reader-{}", frame.id);
                cdp.call(
                    Some(&frame.session),
                    Command::RuntimeReleaseObjectGroup,
                    json!({"objectGroup":group}),
                    deadline,
                )
                .await?;
                let created=cdp.call(Some(&frame.session),Command::RuntimeEvaluate(Effect::None),json!({"contextId":context["executionContextId"],"objectGroup":group,"expression":format!("({COLLECT})({MARKS},{options})",options=cursor.scope.options(PLACES.saturating_sub(cursor.places.iter().sum::<usize>())))}),deadline).await?;
                cursor.readers[index] = Some(
                    created["result"]["objectId"]
                        .as_str()
                        .ok_or_else(|| Failure::new("browser_stale_reference"))?
                        .into(),
                );
            }
            let object = cursor.readers[index].clone().unwrap();
            let summary=cdp.call(Some(&frame.session),Command::RuntimeCallFunction(Effect::None),json!({"objectId":object,"functionDeclaration":"function(nodes,scan){return this.collect(nodes,scan);}","arguments":[{"value":(max_nodes-nodes.len()).min(200)},{"value":(reach-visited).min(5000)}],"returnByValue":true}),deadline).await?;
            let summary=&summary["result"]["value"];
            // A reader that a later observation closed returns no nodes.
            let candidates=summary["nodes"].as_array().ok_or_else(||Failure::new("browser_stale_reference"))?;
            changed|=summary["changed"]==true;
            visited+=summary["scanned"].as_u64().unwrap_or(0) as usize;
            cursor.places[index]=summary["matches"].as_u64().unwrap_or(0) as usize;
            if let Some(values)=summary["omitted"].as_array(){omitted.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));}
            let mut processed=0;let mut partial_chars=0;
            // Elements are described a few at a time, this one and the ones after it.
            let mut described: HashMap<u64, Result<Option<Found>>> = HashMap::new();
            for (position, candidate) in candidates.iter().enumerate() {
                if remaining==0 || nodes.len()>=max_nodes || Instant::now()+Duration::from_millis(200)>=work_until {break;}
                processed+=1;
                // The kind of control that the page makes of the element, if any.
                let marked = Kind::of(candidate);
                let element = match candidate["index"].as_u64() {
                    Some(index) => {
                        if !described.contains_key(&index) {
                            let ahead: Vec<&Value> = candidates[position..].iter().filter(|c| c["index"].is_u64()).take(TOGETHER).collect();
                            let indexes: Vec<u64> = ahead.iter().filter_map(|c| c["index"].as_u64()).collect();
                            let controls: Vec<u64> = ahead.iter().filter(|c| Kind::of(c).is_some()).filter_map(|c| c["index"].as_u64()).collect();
                            described = found(cdp, &frame, &object, &indexes, &controls, deadline).await;
                        }
                        match described.remove(&index).expect("described with the elements after it") {
                        // The accessibility tree leaves out what is hidden. It also leaves out
                        // much of what only looks like a control, and there the page has
                        // already seen that the element shows.
                        Ok(Some(element)) if element.exposed || marked == Some(Kind::Inferred) => Some(element),
                        Ok(_) => continue,
                        // The element left the page after the reader found it. That is a
                        // change to report, unless its whole document went with it.
                        Err(error) if gone(&error) => {
                            current(cdp, &frame, deadline).await?;
                            changed = true;
                            continue;
                        }
                        Err(error) => return Err(error),
                        }
                    }
                    None => None,
                };
                // An element with details is a control: by what the page says of it, or
                // else by what the browser calls it.
                let kind = element.as_ref().and_then(|found| found.data.as_ref()).map(|_| marked.unwrap_or(Kind::Native));
                let (role, full_name) = match &element {
                    // The browser's names for such an element differ between versions, and
                    // are often empty. The page names it.
                    Some(found) if kind == Some(Kind::Inferred) => (
                        if found.exposed && !found.role.is_empty() { found.role.as_str() } else { "generic" },
                        found.data.as_ref().and_then(|data| data["words"].as_str()).unwrap_or(""),
                    ),
                    Some(found) => (found.role.as_str(), found.name.as_str()),
                    None => (candidate["role"].as_str().unwrap_or(""), candidate["name"].as_str().unwrap_or("")),
                };
                let name = clipped(full_name, 512.min(remaining));
                if element.is_some() && name.len() < full_name.len() { omitted.push("text_clipped".into()); }
                if name.is_empty() && kind.is_none() { continue; }
                // The page reader leaves the text out. What the browser alone names, such
                // as a region, is left out here.
                if filter == Filter::Interactive && kind.is_none() && role != "heading" { continue; }
                remaining = remaining.saturating_sub(name.chars().count() + role.len());
                let mut visible = json!({"role":role,"name":name,"frame_id":frame.id});
                if element.is_none() {
                    visible["text_continued"]=candidate["text_continued"].clone();
                    visible["text_remaining"]=(candidate["text_remaining"]==true || name.len()<full_name.len()).into();
                }
                if let (Some(Found { backend, value, data: Some(data), .. }), Some(kind)) = (&element, kind) {
                    visible["kind"] = kind.name().into();
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
                    visible["enabled"] = data["enabled"].clone();
                    visible["editable"] = data["editable"].clone();
                    visible["sensitive"] = (data["type"] == "password").into();
                    visible["file_input"] = (data["type"] == "file").into();
                    // A number field or a slider holds a number, not a string.
                    let value = match value {
                        Value::String(value) => Some(value.clone()),
                        Value::Number(value) => Some(value.as_f64().filter(|v| v.fract() == 0. && v.abs() < 1e15).map_or_else(|| value.to_string(), |v| format!("{v:.0}"))),
                        _ => None,
                    };
                    if data["type"] != "password" && data["type"] != "file" && let Some(value) = value.as_deref() {
                        let shown = clipped(value, remaining.min(512));
                        remaining = remaining.saturating_sub(shown.chars().count());
                        if shown.len() < value.len() { omitted.push("value_clipped".into()); }
                        visible["value"] = shown.into();
                    }
                    if matches!(data["type"].as_str(), Some("checkbox" | "radio")) {
                        visible["checked"] = data["checked"].clone();
                    }
                    if data["steady"] == false {
                        // A reference is bound to the text of the control's record. That text
                        // changed after it was reported, so a reference would be bound to
                        // something other than what the caller was shown.
                        visible["changed_during_read"] = true.into();
                        changed = true;
                    } else {
                        let reference = format!("e_{observation}_{}", refs.len());
                        visible["ref"] = reference.clone().into();
                        refs.insert(reference, Element {
                            frame: frame.clone(), backend: *backend,
                            signature: crate::config::sha256_hex(data["signature"].as_str().unwrap_or("").as_bytes()),
                            role: role.into(), name: crate::config::sha256_hex(full_name.as_bytes()), choices,
                            inferred: kind == Kind::Inferred,
                        });
                    }
                }
                nodes.push(visible);
                if element.is_none() && name.len()<full_name.len() { processed-=1; partial_chars=name.chars().count(); break; }
            }
            let ack=cdp.call(Some(&frame.session),Command::RuntimeCallFunction(Effect::None),json!({"objectId":object,"functionDeclaration":"function(count,chars){return this.ack(count,chars);}","arguments":[{"value":processed},{"value":partial_chars}],"returnByValue":true}),deadline).await?;
            let ack=&ack["result"]["value"];
            if ack["closed"]!=false {return Err(Failure::new("browser_stale_reference"));}
            changed|=ack["changed"]==true;
            current(cdp,&frame,deadline).await?;
            Ok(ack["more"]==true)
        }.await;
        let _ = cdp
            .cleanup()
            .call(
                Some(&frame.session),
                Command::RuntimeReleaseObjectGroup,
                json!({"objectGroup":"rdh-observe"}),
                deadline,
            )
            .await;
        observed.insert(frame.id.clone());
        match result {
            Ok(true) => {}
            Ok(false) => cursor.frame += 1,
            // A subframe that navigated or went away takes only its own content
            // with it. The tab's own document being replaced ends the read.
            Err(error) if index > 0 && gone(&error) => {
                current(cdp, &cursor.frames[0], deadline).await?;
                cursor.omit("frame_changed");
                cursor.frame += 1;
            }
            Err(error) => return Err(error),
        }
    }
    cursor.check(cdp, deadline).await?;
    cursor.changed |= changed;
    cursor.scanned += visited;
    cursor.pages += 1;
    cursor.omitted.extend(omitted);
    cursor.omitted.sort();
    cursor.omitted.dedup();
    let mut omitted = cursor.omitted.clone();
    if cursor.more() {
        omitted.push("observation_budget".into());
    }
    let complete = !cursor.more() && cursor.omitted.is_empty() && !cursor.changed;
    let mut coverage = json!({
        "scope":"document_text_and_controls_open_shadow","complete_within_scope":complete,
        "omitted":omitted,"changed_during_read":cursor.changed,
        "frames_observed":observed.len(),"frames_omitted":cursor.frames.len()-cursor.frame,
        "frames_completed":cursor.frame,"nodes_scanned":visited,"total_nodes_scanned":cursor.scanned,
        "page_index":cursor.pages,"more":cursor.more(),"sequence_complete":complete,
    });
    // A read that was asked for part of the page says which part. It is complete
    // when it holds all of that part.
    match filter {
        Filter::All => {}
        Filter::Interactive => coverage["filter"] = "interactive".into(),
        Filter::Viewport => coverage["filter"] = "viewport".into(),
    }
    if let Some(frame) = &cursor.scope.within {
        coverage["within"] = frame.as_str().into();
    }
    if cursor.scope.find.is_some() {
        coverage["matches"] = cursor.places.iter().sum::<usize>().into();
    }
    Ok((
        json!({"observation_id":observation,"nodes":nodes,"coverage":coverage}),
        refs,
    ))
}

/// Hit-tests a point given in the viewport of a target's root document, which
/// is where content quads and input events are measured.
/// `DOM.getNodeForLocation` takes document coordinates instead, so the scroll
/// offset is added. A point outside the viewport cannot receive input.
async fn node_at(cdp: &Cdp, session: &str, x: f64, y: f64, deadline: Instant) -> Result<Value> {
    let metrics = cdp
        .call(
            Some(session),
            Command::PageGetLayoutMetrics,
            json!({}),
            deadline,
        )
        .await?;
    let view = &metrics["cssLayoutViewport"];
    let (Some(left), Some(top), Some(width), Some(height)) = (
        view["pageX"].as_f64(),
        view["pageY"].as_f64(),
        view["clientWidth"].as_f64(),
        view["clientHeight"].as_f64(),
    ) else {
        return Err(Failure::new("browser_not_actionable"));
    };
    if x < 0. || y < 0. || x >= width || y >= height {
        return Err(Failure::new("browser_not_actionable"));
    }
    cdp.call(
        Some(session),
        Command::DomGetNodeForLocation,
        json!({"x":(x + left).round() as i64,"y":(y + top).round() as i64}),
        deadline,
    )
    .await
}

impl Element {
    pub async fn prepare_file(&self, cdp: &Cdp, top: &Frame, deadline: Instant) -> Result<String> {
        // The consent UI names the top-level site. Cross-origin/nested frames
        // require a separate presentation contract before they can receive files.
        if self.frame.id != top.id || self.frame.session != top.session {
            return Err(Failure::new("browser_upload_denied"));
        }
        let data = self.checked(cdp, deadline).await?;
        if data["tag"] != "INPUT" || data["type"] != "file" {
            return Err(Failure::new("browser_invalid_params"));
        }
        let page = metadata(cdp, &self.frame, deadline).await?;
        origin(page["url"].as_str().unwrap_or(""))
    }
    pub async fn attach_file(
        &self,
        cdp: &Cdp,
        path: &std::path::Path,
        deadline: Instant,
    ) -> Result<Value> {
        cdp.call(
            Some(&self.frame.session),
            Command::DomSetFileInputFiles,
            json!({"backendNodeId":self.backend,"files":[path]}),
            deadline,
        )
        .await?;
        Ok(json!({"file_selected":true,"business_result_verified":false}))
    }
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
                    Command::PageGetLayoutMetrics,
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
                    Command::DomGetFrameOwner,
                    json!({"frameId":child_frame}),
                    deadline,
                )
                .await?;
            let quads = cdp
                .call(
                    Some(parent_session),
                    Command::DomGetContentQuads,
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
            let hit = node_at(cdp, parent_session, x, y, deadline).await?;
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
                Command::DomGetContentQuads,
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
        let hit = node_at(cdp, &self.frame.session, x, y, deadline).await?;
        if hit["frameId"] != self.frame.id {
            return Err(Failure::new("browser_not_actionable"));
        }
        self.ancestor_hit(cdp, x, y, deadline).await?;
        Ok(json!({"x":x,"y":y}))
    }
    async fn checked(&self, cdp: &Cdp, deadline: Instant) -> Result<Value> {
        let root = current(cdp, &self.frame, deadline).await?;
        let mut data = details(cdp, &self.frame, self.backend, deadline).await?;
        if data["connected"] != true
            || crate::config::sha256_hex(data["signature"].as_str().unwrap_or("").as_bytes())
                != self.signature
        {
            return Err(Failure::new("browser_stale_reference"));
        }
        if self.inferred {
            // The page named this element, so the page says whether the name stands.
            if crate::config::sha256_hex(data["words"].as_str().unwrap_or("").as_bytes())
                != self.name
            {
                return Err(Failure::new("browser_stale_reference"));
            }
        } else {
            let ax = cdp
                .call(
                    Some(&self.frame.session),
                    Command::AccessibilityGetPartialAXTree,
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
        }
        if data["enabled"] != true || data["visible"] != true {
            return Err(Failure::new("browser_not_actionable"));
        }
        // The tab's top document: the root frame of a target that has no parent.
        data["top"] = (root && self.frame.ancestors.is_empty()).into();
        data["inferred"] = self.inferred.into();
        Ok(data)
    }

    /// Sets the value of a field whose value is picked. Without `apply` it only
    /// asks whether the field can hold `value`, which changes nothing.
    async fn set(
        &self,
        cdp: &Cdp,
        value: &Value,
        apply: bool,
        deadline: Instant,
    ) -> Result<Option<bool>> {
        let session = Some(self.frame.session.as_str());
        let object = object(cdp, &self.frame, self.backend, deadline).await?;
        let params = json!({"objectId":object,"functionDeclaration":SET,
            "arguments":[{"value":value},{"value":apply}],"returnByValue":true});
        let result = if apply {
            cdp.call(
                session,
                Command::RuntimeCallFunction(Effect::Page),
                params,
                deadline,
            )
            .await
        } else {
            cdp.call(
                session,
                Command::RuntimeCallFunction(Effect::None),
                params,
                deadline,
            )
            .await
        };
        let _ = cdp
            .cleanup()
            .call(
                session,
                Command::RuntimeReleaseObject,
                json!({"objectId":object}),
                deadline,
            )
            .await;
        Ok(result?["result"]["value"].as_bool())
    }

    pub async fn act(&self, cdp: &Cdp, action: &Value, deadline: Instant) -> Result<Value> {
        let data = self.checked(cdp, deadline).await?;
        // Nothing has been sent yet, so an action that does not suit this
        // element is a plain rejection instead of an uncertain outcome.
        if precheck(action, &data, &self.choices)? {
            return Ok(json!({"changed":false}));
        }
        // An element that only looks like a control can hold real ones, as a card
        // holds its delete button. A click meant for the card must not be taken
        // by one of them.
        let taken = |data: &Value| {
            action["kind"] == "click" && data["inferred"] == true && data["inner"] == true
        };
        // Scrolling into view is an effect command, and it does nothing for an
        // element that is already fully in view. Skip it there, so that a
        // target which something covers is refused before any effect. Only the
        // top document can tell: a subframe sees just its own viewport. An
        // element that is neither hit nor painted at its click point is clipped
        // by something the page function cannot see, and still scrolls.
        if data["top"] == true
            && data["in_view"] == true
            && (data["hit"] == true || data["covered"] == true)
        {
            if data["covered"] == true || taken(&data) {
                return Err(Failure::new("browser_not_actionable"));
            }
        } else {
            cdp.call(
                Some(&self.frame.session),
                Command::DomScrollIntoViewIfNeeded,
                json!({"backendNodeId":self.backend}),
                deadline,
            )
            .await?;
        }
        let first = self.point(cdp, deadline).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let data = self.checked(cdp, deadline).await?;
        let point = self.point(cdp, deadline).await?;
        if data["hit"] != true
            || first["x"] != point["x"]
            || first["y"] != point["y"]
            || taken(&data)
        {
            return Err(Failure::new("browser_not_actionable"));
        }
        // The element can change while the page settles.
        if precheck(action, &data, &self.choices)? {
            return Ok(json!({"changed":false}));
        }
        let session = Some(self.frame.session.as_str());
        match action["kind"].as_str().unwrap_or("") {
            "select" => {
                let value = action["value"].as_str().unwrap_or("");
                let object = object(cdp, &self.frame, self.backend, deadline).await?;
                let result = cdp.call(session,Command::RuntimeCallFunction(Effect::Page),json!({"objectId":object,
                    "functionDeclaration":SELECT,"arguments":[{"value":value}],"returnByValue":true}),deadline).await;
                let _ = cdp
                    .cleanup()
                    .call(
                        session,
                        Command::RuntimeReleaseObject,
                        json!({"objectId":object}),
                        deadline,
                    )
                    .await;
                if result?["result"]["value"] != true {
                    return Err(Failure::unknown("browser_not_actionable"));
                }
            }
            "click" | "check" => {
                cdp.input_pair(session,InputPair::Mouse,json!({"type":"mousePressed","x":point["x"],"y":point["y"],"button":"left","clickCount":1}),json!({"type":"mouseReleased","x":point["x"],"y":point["y"],"button":"left","clickCount":1}),deadline).await?;
            }
            "fill" if picked(&data) => {
                // Asked before anything is sent, so that a value the field cannot
                // hold is a plain rejection.
                if self.set(cdp, &action["text"], false, deadline).await? != Some(true) {
                    return Err(Failure::new("browser_invalid_params"));
                }
                if self.set(cdp, &action["text"], true, deadline).await? != Some(true) {
                    return Err(Failure::unknown("browser_not_actionable"));
                }
            }
            "fill" => {
                cdp.call(
                    session,
                    Command::DomFocus,
                    json!({"backendNodeId":self.backend}),
                    deadline,
                )
                .await?;
                // Input.insertText follows the browser's editing path, including frontend input events.
                cdp.input_pair(session,InputPair::Key,json!({"type":"keyDown","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65}),json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65}),deadline).await?;
                cdp.call(
                    session,
                    Command::InputInsertText,
                    json!({"text":action["text"]}),
                    deadline,
                )
                .await?;
            }
            "press" => {
                let key = action["key"].as_str().unwrap_or("");
                let code = key_code(key).ok_or_else(|| Failure::new("browser_invalid_params"))?;
                cdp.call(
                    session,
                    Command::DomFocus,
                    json!({"backendNodeId":self.backend}),
                    deadline,
                )
                .await?;
                cdp.input_pair(session,InputPair::Key,json!({"type":"keyDown","key":key,"windowsVirtualKeyCode":code,"text":if key=="Enter"{"\r"}else{""}}),json!({"type":"keyUp","key":key,"windowsVirtualKeyCode":code}),deadline).await?;
            }
            _ => return Err(Failure::new("browser_invalid_params")),
        }
        Ok(json!({"input_acknowledged":true,"business_result_verified":false}))
    }
}

/// Decides, without any page effect, whether `action` suits the element that
/// `data` describes. `Ok(true)` means the element already holds the requested
/// state and needs no input.
fn precheck(action: &Value, data: &Value, choices: &[String]) -> Result<bool> {
    let invalid = || Failure::new("browser_invalid_params");
    let kind = action["kind"].as_str().unwrap_or("");
    // An element that only looks like a control takes clicks and keys, and
    // nothing that needs a real field, box or list.
    if data["inferred"] == true && !matches!(kind, "click" | "press") {
        return Err(invalid());
    }
    match kind {
        "click" => {}
        "press" => {
            key_code(action["key"].as_str().unwrap_or("")).ok_or_else(invalid)?;
        }
        "check" => {
            if !matches!(data["type"].as_str(), Some("checkbox" | "radio")) {
                return Err(invalid());
            }
            return Ok(data["checked"] == action["checked"]);
        }
        "select" => {
            let value = action["value"].as_str().unwrap_or("");
            if data["tag"] != "SELECT"
                || data["select"]["multiple"] == true
                || !choices.iter().any(|v| v == value)
            {
                return Err(invalid());
            }
        }
        "fill" => {
            if data["type"] == "password" {
                return Err(Failure::new("browser_sensitive_input"));
            }
            // A read-only field still takes clicks and keys, only not text.
            if data["readonly"] == true {
                return Err(Failure::new("browser_not_actionable"));
            }
            if data["editable"] != true {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(false)
}

fn key_code(key: &str) -> Option<i64> {
    Some(match key {
        "Enter" => 13,
        "Tab" => 9,
        "Escape" => 27,
        "Backspace" => 8,
        "ArrowDown" => 40,
        "ArrowUp" => 38,
        "ArrowLeft" => 37,
        "ArrowRight" => 39,
        _ => return None,
    })
}

pub fn origin(value: &str) -> Result<String> {
    if !url_allowed(value) {
        return Err(Failure::new("browser_url_denied"));
    }
    Ok(url::Url::parse(value)
        .map_err(super::io_error)?
        .origin()
        .ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejection(
        action: Value,
        data: &Value,
        choices: &[&str],
    ) -> Option<(&'static str, &'static str)> {
        let choices: Vec<String> = choices.iter().map(|v| (*v).to_owned()).collect();
        precheck(&action, data, &choices)
            .err()
            .map(|error| (error.code, error.state))
    }

    #[test]
    fn unsuitable_actions_are_rejected_without_a_page_effect() {
        let text = json!({"tag":"INPUT","type":"text","editable":true});
        let password = json!({"tag":"INPUT","type":"password","editable":true});
        let button = json!({"tag":"BUTTON","type":"submit","editable":false});
        let single = json!({"tag":"SELECT","type":"select-one","editable":false,"select":{"multiple":false}});
        let multiple = json!({"tag":"SELECT","type":"select-multiple","editable":false,"select":{"multiple":true}});
        let invalid = Some(("browser_invalid_params", "not_sent"));

        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &text, &[]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &password, &[]),
            Some(("browser_sensitive_input", "not_sent"))
        );
        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &button, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &single, &["red"]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"blue"}), &single, &["red"]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &multiple, &["red"]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &text, &["red"]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"check","checked":true}), &text, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"press","key":"Enter"}), &button, &[]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"press","key":"F5"}), &button, &[]),
            invalid
        );
        assert_eq!(rejection(json!({"kind":"click"}), &button, &[]), None);
        assert_eq!(
            rejection(json!({"kind":"attach_file"}), &button, &[]),
            invalid
        );
    }

    #[test]
    fn an_element_that_only_looks_like_a_control_takes_clicks_and_keys_only() {
        // The page would let this one be edited and checked. It is still not a field.
        let card = json!({"tag":"DIV","type":"checkbox","editable":true,"inferred":true,
            "select":{"multiple":false}});
        let invalid = Some(("browser_invalid_params", "not_sent"));
        assert_eq!(rejection(json!({"kind":"click"}), &card, &[]), None);
        assert_eq!(
            rejection(json!({"kind":"press","key":"Enter"}), &card, &[]),
            None
        );
        assert_eq!(
            rejection(json!({"kind":"fill","text":"x"}), &card, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"check","checked":true}), &card, &[]),
            invalid
        );
        assert_eq!(
            rejection(json!({"kind":"select","value":"red"}), &card, &["red"]),
            invalid
        );
    }

    fn frame(id: &str, loader: &str, url: &str) -> Frame {
        Frame {
            session: "session".into(),
            id: id.into(),
            loader: loader.into(),
            url: url.into(),
            fragment: String::new(),
            ancestors: Vec::new(),
        }
    }

    #[tokio::test]
    async fn elements_are_described_together_and_each_has_its_own_outcome() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
        let (client, browser) = tokio::io::duplex(65536);
        let (read, write) = tokio::io::split(client);
        let cdp = Cdp::new(read, write);
        let frame = frame("top", "1", "https://shop.test/orders");
        let mut browser = BufReader::new(browser);
        async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
            let mut frame = Vec::new();
            browser.read_until(0, &mut frame).await.unwrap();
            serde_json::from_slice(&frame[..frame.len() - 1]).unwrap()
        }
        async fn answer(browser: &mut BufReader<DuplexStream>, request: &Value, result: Value) {
            let mut reply =
                serde_json::to_vec(&json!({"id":request["id"],"result":result})).unwrap();
            reply.push(0);
            browser.get_mut().write_all(&reply).await.unwrap();
        }
        // The page marks the first and the last of four elements as controls. The
        // pretend browser answers nothing until it has the four requests for the
        // elements and the one for the details of those two. Asked one at a time,
        // it would never get the second.
        let pretend = async {
            let mut asked = Vec::new();
            for _ in 0..5 {
                asked.push(request(&mut browser).await);
            }
            let details = asked.pop().unwrap();
            assert_eq!(details["params"]["arguments"][0]["value"], json!([0, 3]));
            // The last element left the page before its details were read.
            answer(
                &mut browser,
                &details,
                json!({"result":{"value":[{"connected":true,"signature":"s"},null]}}),
            )
            .await;
            for handle in &asked {
                assert_eq!(handle["method"], "Runtime.callFunctionOn", "{handle}");
                let index = handle["params"]["arguments"][0]["value"].as_u64().unwrap();
                answer(
                    &mut browser,
                    handle,
                    json!({"result":{"objectId":format!("node-{index}")}}),
                )
                .await;
            }
            asked.clear();
            for _ in 0..4 {
                asked.push(request(&mut browser).await);
            }
            for describe in &asked {
                assert_eq!(describe["method"], "DOM.describeNode", "{describe}");
                let object = describe["params"]["objectId"].as_str().unwrap();
                let index: i64 = object.trim_start_matches("node-").parse().unwrap();
                answer(
                    &mut browser,
                    describe,
                    json!({"node":{"backendNodeId":10 + index}}),
                )
                .await;
            }
            asked.clear();
            for _ in 0..4 {
                asked.push(request(&mut browser).await);
            }
            for tree in &asked {
                assert_eq!(tree["method"], "Accessibility.getPartialAXTree", "{tree}");
                let backend = tree["params"]["backendNodeId"].as_i64().unwrap();
                let node = match backend {
                    10 => {
                        json!({"backendDOMNodeId":10,"role":{"value":"button"},"name":{"value":"Save"}})
                    }
                    // The page does not mark the second, and the browser calls it a link.
                    11 => {
                        json!({"backendDOMNodeId":11,"role":{"value":"link"},"name":{"value":"Open"}})
                    }
                    12 => json!({"backendDOMNodeId":12,"ignored":true}),
                    _ => {
                        json!({"backendDOMNodeId":13,"role":{"value":"textbox"},"name":{"value":"Name"}})
                    }
                };
                answer(&mut browser, tree, json!({"nodes":[node]})).await;
            }
            // The page is then asked for the details of what the browser alone calls
            // a control.
            let details = request(&mut browser).await;
            assert_eq!(details["method"], "Runtime.callFunctionOn", "{details}");
            assert_eq!(details["params"]["arguments"][0]["value"], json!([1]));
            answer(
                &mut browser,
                &details,
                json!({"result":{"value":[{"connected":true,"signature":"t"}]}}),
            )
            .await;
        };
        let described = found(
            &cdp,
            &frame,
            "reader",
            &[0, 1, 2, 3],
            &[0, 3],
            Instant::now() + Duration::from_secs(2),
        );
        let (mut described, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(described, pretend)
        })
        .await
        .expect("the browser must be asked about all four elements at once");
        cdp.stop();

        let button = described.remove(&0).unwrap().unwrap().unwrap();
        assert_eq!(
            (button.backend, button.role.as_str(), button.name.as_str()),
            (10, "button", "Save")
        );
        assert_eq!(button.data.unwrap()["signature"], "s");
        let link = described.remove(&1).unwrap().unwrap().unwrap();
        assert_eq!(link.role, "link");
        assert_eq!(link.data.unwrap()["signature"], "t");
        // The accessibility tree ignores the third, which the page does not mark.
        let ignored = described.remove(&2).unwrap().unwrap().unwrap();
        assert!(!ignored.exposed);
        assert!(ignored.data.is_none());
        let gone = described.remove(&3).unwrap().err().unwrap();
        assert_eq!(
            (gone.code, gone.state),
            ("browser_stale_reference", "not_sent")
        );
    }

    #[test]
    fn a_sequence_continues_while_the_tab_shows_the_document_it_began_on() {
        let begun = [
            frame("top", "1", "https://shop.test/orders"),
            frame("ad", "2", "https://ads.test/first"),
        ];
        let mut cursor = Cursor::new(&begun, Scope::default()).unwrap();
        assert!(cursor.matches(&begun));
        cursor.compare(&begun);
        assert!(cursor.omitted.is_empty());

        // A subframe that navigated, or one that went away, is named as left out.
        let rotated = [
            begun[0].clone(),
            frame("ad", "3", "https://ads.test/second"),
        ];
        assert!(cursor.matches(&rotated));
        assert!(cursor.matches(&begun[..1]));
        cursor.compare(&rotated);
        cursor.compare(&begun[..1]);
        assert_eq!(cursor.omitted, ["frame_changed"]);

        // The tab's own document being replaced, or its address changing, ends it.
        assert!(!cursor.matches(&[frame("top", "9", "https://shop.test/orders")]));
        assert!(!cursor.matches(&[frame("top", "1", "https://shop.test/orders/7")]));
        assert!(!cursor.matches(&[]));
    }

    #[test]
    fn a_sequence_that_reads_one_frame_looks_at_that_frame_only() {
        let begun = [
            frame("top", "1", "https://shop.test/orders"),
            frame("ad", "2", "https://ads.test/first"),
            frame("pay", "3", "https://pay.test/form"),
        ];
        let within = |id: &str| Scope {
            within: Some(id.into()),
            ..Scope::default()
        };
        let mut cursor = Cursor::new(&begun, within("pay")).unwrap();
        assert_eq!(cursor.frames.len(), 1);
        assert_eq!(cursor.frames[0].id, "pay");

        // Another frame that navigates or goes away is none of its business.
        let rotated = [
            begun[0].clone(),
            frame("ad", "9", "https://ads.test/second"),
            begun[2].clone(),
        ];
        assert!(cursor.matches(&rotated));
        cursor.compare(&rotated);
        cursor.compare(&[begun[0].clone(), begun[2].clone()]);
        assert!(cursor.omitted.is_empty());
        // Its own frame navigating is.
        cursor.compare(&[begun[0].clone(), frame("pay", "4", "https://pay.test/done")]);
        assert_eq!(cursor.omitted, ["frame_changed"]);
        // It still ends with the tab's own document.
        assert!(!cursor.matches(&[
            frame("top", "9", "https://shop.test/orders"),
            begun[2].clone()
        ]));

        // A frame that the page does not hold cannot be read.
        let missing = Cursor::new(&begun, within("gone")).err().unwrap();
        assert_eq!(
            (missing.code, missing.state),
            ("browser_stale_reference", "not_sent")
        );
    }

    #[test]
    fn a_checkbox_that_already_matches_needs_no_input() {
        let checked = json!({"tag":"INPUT","type":"checkbox","checked":true});
        assert!(precheck(&json!({"kind":"check","checked":true}), &checked, &[]).unwrap());
        assert!(!precheck(&json!({"kind":"check","checked":false}), &checked, &[]).unwrap());
    }

    #[test]
    fn a_read_only_field_takes_clicks_and_keys_but_not_text() {
        let field = json!({"tag":"INPUT","type":"text","editable":false,"readonly":true});
        let secret = json!({"tag":"INPUT","type":"password","editable":false,"readonly":true});
        let fill = || json!({"kind":"fill","text":"x"});
        assert_eq!(rejection(json!({"kind":"click"}), &field, &[]), None);
        assert_eq!(
            rejection(json!({"kind":"press","key":"Enter"}), &field, &[]),
            None
        );
        assert_eq!(
            rejection(fill(), &field, &[]),
            Some(("browser_not_actionable", "not_sent"))
        );
        assert_eq!(
            rejection(fill(), &secret, &[]),
            Some(("browser_sensitive_input", "not_sent"))
        );
    }
}
