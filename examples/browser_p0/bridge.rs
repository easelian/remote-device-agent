//! Loopback-only handoff experiment. Not an RDH authentication/media endpoint.
use super::{Result, cdp::Cdp};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

struct Control {
    human: bool,
    epoch: u64,
    frame: u64,
    sequence: u64,
    shown: Instant,
}
struct StateData {
    cdp: Cdp,
    session: String,
    key: String,
    origin: String,
    control: Mutex<Control>,
}
pub struct Bridge {
    pub url: String,
    pub key: String,
    pub epoch: u64,
    state: Arc<StateData>,
    task: JoinHandle<()>,
}
type HttpResult = std::result::Result<Json<Value>, (StatusCode, &'static str)>;

fn authorize(
    state: &StateData,
    headers: &HeaderMap,
) -> std::result::Result<(), (StatusCode, &'static str)> {
    if headers.get("x-p0-key").and_then(|v| v.to_str().ok()) != Some(&state.key)
        || headers.get("origin").and_then(|v| v.to_str().ok()) != Some(&state.origin)
    {
        return Err((StatusCode::FORBIDDEN, "unauthorized"));
    }
    Ok(())
}

async fn frame(State(state): State<Arc<StateData>>, headers: HeaderMap) -> HttpResult {
    authorize(&state, &headers)?;
    let mut control = state
        .control
        .try_lock()
        .map_err(|_| (StatusCode::CONFLICT, "control_busy"))?;
    if !control.human {
        return Err((StatusCode::CONFLICT, "not_human"));
    }
    let result = state
        .cdp
        .call(
            Some(&state.session),
            "Page.captureScreenshot",
            json!({"format":"jpeg","quality":60}),
        )
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "capture_failed"))?;
    let data = result["data"]
        .as_str()
        .ok_or((StatusCode::BAD_GATEWAY, "missing_image"))?;
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "bad_image"))?;
    if bytes.len() > 512 * 1024 {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "image_limit"));
    }
    control.frame += 1;
    control.shown = Instant::now();
    Ok(Json(
        json!({"epoch":control.epoch,"frame":control.frame,"width":1280,"height":720,"jpeg":data}),
    ))
}

async fn input(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    Json(v): Json<Value>,
) -> HttpResult {
    authorize(&state, &headers)?;
    let mut control = state
        .control
        .try_lock()
        .map_err(|_| (StatusCode::CONFLICT, "control_busy"))?;
    if !control.human
        || v["epoch"].as_u64() != Some(control.epoch)
        || v["frame"].as_u64() != Some(control.frame)
        || control.frame == 0
        || control.shown.elapsed() > Duration::from_secs(3)
        || state.cdp.pending() != 0
        || !state.cdp.connected()
    {
        return Err((StatusCode::CONFLICT, "stale_or_busy"));
    }
    if v["sequence"].as_u64() != Some(control.sequence + 1) {
        return Err((StatusCode::CONFLICT, "duplicate_or_out_of_order"));
    }
    // Once accepted, an uncertain request cannot reuse its sequence to emit input again.
    control.sequence += 1;
    // The fixture handles one request at a time and does not retry lost replies.
    let result = match v["type"].as_str() {
        Some("text") => {
            let text = v["text"]
                .as_str()
                .ok_or((StatusCode::BAD_REQUEST, "missing_text"))?;
            if text.len() > 4096 {
                return Err((StatusCode::PAYLOAD_TOO_LARGE, "text_limit"));
            }
            state
                .cdp
                .call(
                    Some(&state.session),
                    "Input.insertText",
                    json!({"text":text}),
                )
                .await
        }
        Some("click") => {
            let x = v["x"]
                .as_f64()
                .ok_or((StatusCode::BAD_REQUEST, "missing_x"))?;
            let y = v["y"]
                .as_f64()
                .ok_or((StatusCode::BAD_REQUEST, "missing_y"))?;
            if !(0.0..1280.).contains(&x) || !(0.0..720.).contains(&y) {
                return Err((StatusCode::BAD_REQUEST, "outside_viewport"));
            }
            for kind in ["mousePressed", "mouseReleased"] {
                state
                    .cdp
                    .call(
                        Some(&state.session),
                        "Input.dispatchMouseEvent",
                        json!({"type":kind,"x":x,"y":y,"button":"left","clickCount":1}),
                    )
                    .await
                    .map_err(|_| (StatusCode::BAD_GATEWAY, "input_outcome_unknown"))?;
            }
            Ok(json!({}))
        }
        _ => return Err((StatusCode::BAD_REQUEST, "unknown_input")),
    };
    result.map_err(|_| (StatusCode::BAD_GATEWAY, "input_outcome_unknown"))?;
    Ok(Json(json!({"accepted":true,"epoch":control.epoch})))
}

impl Bridge {
    pub async fn start(cdp: Cdp, session: &str) -> Result<Self> {
        if cdp.pending() != 0 || !cdp.connected() {
            return Err("handoff_not_quiescent".into());
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let mut key = [0u8; 24];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
            .map_err(|_| "random_failed")?;
        let key = key.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let state = Arc::new(StateData {
            cdp,
            session: session.into(),
            key: key.clone(),
            origin: url.clone(),
            control: Mutex::new(Control {
                human: true,
                epoch: 2,
                frame: 0,
                sequence: 0,
                shown: Instant::now(),
            }),
        });
        let router = Router::new()
            .route("/", get(|| async { Html(VIEWER) }))
            .route("/frame", post(frame))
            .route("/input", post(input))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .with_state(state.clone());
        let task = tokio::spawn(async {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            url,
            key,
            epoch: 2,
            state,
            task,
        })
    }
    pub async fn return_to_ai(&self) -> Result<()> {
        let mut control = self.state.control.lock().await;
        if self.state.cdp.pending() != 0 || !self.state.cdp.connected() {
            return Err("handoff_not_quiescent".into());
        }
        control.human = false;
        control.epoch += 1;
        control.frame = 0;
        Ok(())
    }
    pub async fn ai_observe(&self) -> Result<Value> {
        let control = self.state.control.lock().await;
        if control.human {
            return Err("human_disclosure_barrier".into());
        }
        super::page::observe(&self.state.cdp, &self.state.session).await
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const VIEWER: &str = r#"<!doctype html><html><meta charset="utf-8"><title>RDH P0 local handoff</title>
<style>body{font:16px sans-serif;margin:24px}img{max-width:100%;border:1px solid #aaa}button,input{padding:8px;margin:6px}</style>
<h1>Local browser handoff prototype</h1><p>Synthetic test pages only. No Hub authentication, reconnect, or production approval flow.</p>
<p id="status">Ready</p><button id="refresh">Refresh frame</button><input id="text" type="password" autocomplete="off" placeholder="Text to insert"><button id="send">Insert text</button><br><img id="screen" alt="Remote test page">
<script>
const key=location.hash.slice(1);history.replaceState(null,'',location.pathname);let frame,busy=false,sequence=0;
const screen=document.getElementById('screen'),text=document.getElementById('text'),status=document.getElementById('status');
async function request(path,body){const r=await fetch(path,{method:'POST',headers:{'x-p0-key':key,'Content-Type':'application/json'},body:JSON.stringify(body||{})});if(!r.ok)throw Error('Rejected '+r.status+': '+await r.text());return r.json()}
async function refresh(){frame=await request('/frame');screen.src='data:image/jpeg;base64,'+frame.jpeg;status.textContent='Frame '+frame.frame}
async function act(payload){if(busy||!frame)return;busy=true;try{await request('/input',{...payload,epoch:frame.epoch,frame:frame.frame,sequence:++sequence});text.value='';await refresh()}catch(e){status.textContent=e.message}finally{busy=false}}
screen.onclick=e=>{const r=screen.getBoundingClientRect();act({type:'click',x:(e.clientX-r.left)*1280/r.width,y:(e.clientY-r.top)*720/r.height})};
document.getElementById('send').onclick=()=>act({type:'text',text:text.value});
document.getElementById('refresh').onclick=()=>refresh().catch(e=>status.textContent=e.message);refresh().catch(e=>status.textContent=e.message);
</script></html>"#;
