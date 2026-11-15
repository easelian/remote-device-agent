//! Text stays bound to the focused control seen in the user's image.
use super::*;
use crate::browser::cdp::Command;
use crate::browser::control::Effect;

pub(super) struct Focus {
    frame: page::Frame,
    backend: i64,
}

const FOCUSED: &str = r#"(()=>{
 if(!document.hasFocus())return null;
 let e=document.activeElement;while(e?.shadowRoot?.activeElement)e=e.shadowRoot.activeElement;
 return e&&((e instanceof HTMLInputElement&&!['button','submit','reset','checkbox','radio','file','hidden','range','color'].includes(e.type))||e instanceof HTMLTextAreaElement)&&!e.disabled&&!e.readOnly?e:null;
})()"#;

// This fixed isolated-world function edits the captured node, never whichever
// field happens to have focus after a navigation. It returns no field value.
const INSERT: &str = r#"function(text){
 const focused=()=>{let e=document.activeElement;while(e?.shadowRoot?.activeElement)e=e.shadowRoot.activeElement;return e;};
 const valid=()=>this.isConnected&&this.ownerDocument===document&&document.hasFocus()&&focused()===this&&!this.disabled&&!this.readOnly;
 if(!valid())return 'stale';
 const proto=this instanceof HTMLInputElement?HTMLInputElement.prototype:this instanceof HTMLTextAreaElement?HTMLTextAreaElement.prototype:null;
 if(!proto)return 'stale';
 if(!this.dispatchEvent(new InputEvent('beforeinput',{bubbles:true,composed:true,cancelable:true,inputType:'insertText',data:text})))return 'prevented';
 if(!valid())return 'changed';
 const old=this.value,start=this.selectionStart??old.length,end=this.selectionEnd??old.length;
 Object.getOwnPropertyDescriptor(proto,'value').set.call(this,old.slice(0,start)+text+old.slice(end));
 try{this.setSelectionRange(start+text.length,start+text.length);}catch{}
 this.dispatchEvent(new InputEvent('input',{bubbles:true,composed:true,inputType:'insertText',data:text}));
 return 'inserted';
}"#;

pub(super) async fn capture(
    cdp: &Cdp,
    frames: &[page::Frame],
    deadline: Instant,
) -> Result<Option<Focus>> {
    for frame in frames {
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
        let value = cdp.call(Some(&frame.session), Command::RuntimeEvaluate(Effect::None), json!({
            "contextId":context["executionContextId"],"expression":FOCUSED,"objectGroup":"rdh-human-focus",
        }), deadline).await?;
        let Some(object) = value["result"]["objectId"].as_str() else {
            continue;
        };
        let node = cdp
            .call(
                Some(&frame.session),
                Command::DomDescribeNode,
                json!({"objectId":object,"depth":0}),
                deadline,
            )
            .await;
        let _ = cdp
            .cleanup()
            .call(
                Some(&frame.session),
                Command::RuntimeReleaseObjectGroup,
                json!({"objectGroup":"rdh-human-focus"}),
                deadline,
            )
            .await;
        return node.and_then(|node| {
            node["node"]["backendNodeId"]
                .as_i64()
                .map(|backend| {
                    Some(Focus {
                        frame: frame.clone(),
                        backend,
                    })
                })
                .ok_or_else(|| Failure::new("browser_stale_frame"))
        });
    }
    Ok(None)
}

impl Focus {
    pub(super) async fn insert(&self, cdp: &Cdp, text: &Value, deadline: Instant) -> Result<()> {
        let session = Some(self.frame.session.as_str());
        if !page::frame_tree(cdp, &self.frame.session, deadline)
            .await?
            .iter()
            .any(|frame| frame.id == self.frame.id && frame.loader == self.frame.loader)
        {
            return Err(Failure::new("browser_stale_frame"));
        }
        let context = cdp.call(session,Command::PageCreateIsolatedWorld,json!({
            "frameId":self.frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false,
        }),deadline).await?;
        let node = cdp.call(session,Command::DomResolveNode,json!({"backendNodeId":self.backend,"executionContextId":context["executionContextId"],"objectGroup":"rdh-human-focus"}),deadline).await?;
        let object = node["object"]["objectId"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_stale_frame"))?;
        let result = cdp
            .call(
                session,
                Command::RuntimeCallFunction(Effect::Page),
                json!({"objectId":object,"functionDeclaration":INSERT,
            "arguments":[{"value":text}],"returnByValue":true}),
                deadline,
            )
            .await;
        let _ = cdp
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
        match result["result"]["value"].as_str() {
            Some("inserted") => Ok(()),
            Some("stale") => Err(Failure::new("browser_stale_frame")),
            _ => Err(Failure::unknown("browser_not_actionable")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    async fn request(browser: &mut BufReader<DuplexStream>) -> Value {
        let mut frame = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), browser.read_until(0, &mut frame))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.pop(), Some(0));
        serde_json::from_slice(&frame).unwrap()
    }

    async fn send(browser: &mut BufReader<DuplexStream>, value: Value) {
        let mut frame = serde_json::to_vec(&value).unwrap();
        frame.push(0);
        browser.get_mut().write_all(&frame).await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_human_insertion_keeps_input_handoff_blocked_until_reply() {
        let (client, browser) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let cdp = Cdp::new(read, write);
        let control = Arc::new(Control::new());
        let epoch = control.change(true);
        let (human, effects) = cdp.human(control, epoch).track_effects();
        let focus = Focus {
            frame: page::Frame {
                session: "page".into(),
                id: "frame".into(),
                loader: "document".into(),
                url: "https://fixture.invalid/".into(),
                fragment: String::new(),
                ancestors: Vec::new(),
            },
            backend: 7,
        };
        let task = tokio::spawn(async move {
            focus
                .insert(
                    &human,
                    &json!("fixture"),
                    Instant::now() + Duration::from_secs(30),
                )
                .await
        });
        let mut browser = BufReader::new(browser);
        for (method, result) in [
            (
                "Page.getFrameTree",
                json!({"frameTree":{"frame":{"id":"frame","loaderId":"document"}}}),
            ),
            ("Page.createIsolatedWorld", json!({"executionContextId":1})),
            ("DOM.resolveNode", json!({"object":{"objectId":"node"}})),
        ] {
            let command = request(&mut browser).await;
            assert_eq!(command["method"], method);
            send(&mut browser, json!({"id":command["id"],"result":result})).await;
        }
        let insert = request(&mut browser).await;
        assert_eq!(insert["method"], "Runtime.callFunctionOn");
        assert_eq!(insert["params"]["functionDeclaration"], INSERT);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Human release and AI resume both rely on this barrier. Cancelling
        // the caller cannot prove the browser stopped editing the field.
        assert_eq!(cdp.pending(), 1);
        assert!(effects.load(Ordering::Acquire));
        let mut events = cdp.events();
        send(
            &mut browser,
            json!({"id":insert["id"],"result":{"result":{"value":"inserted"}}}),
        )
        .await;
        send(
            &mut browser,
            json!({"method":"Page.lifecycleEvent","params":{"name":"test_barrier"}}),
        )
        .await;
        assert_eq!(
            events.recv().await.unwrap()["params"]["name"],
            "test_barrier"
        );
        assert_eq!(cdp.pending(), 0);
        assert!(effects.load(Ordering::Acquire));
        cdp.stop();
    }
}
