use super::Result;
use axum::{
    Json, Router,
    extract::State,
    response::Html,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Default)]
pub struct Records {
    pub saves: usize,
    pub value: String,
    pub frame_clicks: usize,
    pub inputs: usize,
}

pub struct Fixture {
    pub port: u16,
    pub records: Arc<Mutex<Records>>,
    task: JoinHandle<()>,
}

impl Fixture {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let records = Arc::new(Mutex::new(Records::default()));
        let page = PAGE.replace("FRAME_PORT", &port.to_string());
        let router = Router::new()
            .route(
                "/",
                get(move || {
                    let page = page.clone();
                    async move { Html(page) }
                }),
            )
            .route("/frame", get(|| async { Html(FRAME) }))
            .route(
                "/save",
                post(
                    |State(s): State<Arc<Mutex<Records>>>, Json(v): Json<Value>| async move {
                        let mut s = s.lock().unwrap();
                        s.saves += 1;
                        s.value = v["value"].as_str().unwrap_or("").into();
                        Json(json!({"saved":true}))
                    },
                ),
            )
            .route(
                "/frame-click",
                post(|State(s): State<Arc<Mutex<Records>>>| async move {
                    s.lock().unwrap().frame_clicks += 1;
                    Json(json!({"clicked":true}))
                }),
            )
            .route(
                "/input",
                post(|State(s): State<Arc<Mutex<Records>>>| async move {
                    // Count only; do not store input text (including the synthetic password).
                    s.lock().unwrap().inputs += 1;
                    Json(json!({"observed":true}))
                }),
            )
            .with_state(records.clone());
        let task = tokio::spawn(async {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self {
            port,
            records,
            task,
        })
    }
    pub fn url(&self) -> String {
        format!("http://a.test:{}/", self.port)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

const PAGE: &str = r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8">
<title>RDH P0 — local test data</title>
<style>body{font:20px sans-serif;margin:40px;max-width:1000px} input,button{font:inherit;padding:10px;margin:8px}iframe{width:600px;height:180px}#overlay{display:none;position:fixed;inset:0;background:#7779;z-index:10}</style>
<h1>RDH browser prototype</h1><p>Local synthetic fixture. No real accounts.</p>
<form id="form"><label>Contact <input id="contact" aria-label="Contact"></label>
<button id="save" type="submit">Save record</button></form><output id="status"></output>
<p><label>Password <input type="password" id="password" aria-label="Password" autocomplete="off"></label></p>
<button id="dynamic" type="button">Replaceable</button><div id="shadow"></div>
<iframe title="Cross-site frame" src="http://b.test:FRAME_PORT/frame"></iframe>
<div id="overlay">Obscured target</div>
<script>
form.addEventListener('submit', async event => {event.preventDefault();await fetch('/save',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({value:contact.value})});document.getElementById('status').textContent='Saved';});
document.addEventListener('input',()=>fetch('/input',{method:'POST'}));
shadow.attachShadow({mode:'open'}).innerHTML='<button>Shadow action</button>';
window.replaceTarget=()=>{const n=dynamic.cloneNode(true);dynamic.replaceWith(n)};
</script></html>"#;

const FRAME: &str = r#"<!doctype html><html><meta charset="utf-8"><title>Independent frame</title>
<style>body{font:18px sans-serif}button{padding:12px;font:inherit}</style>
<h2>Cross-site context</h2><button id="frameButton">Frame action</button>
<script>frameButton.onclick=()=>fetch('/frame-click',{method:'POST'});</script></html>"#;
