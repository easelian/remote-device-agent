//! Text stays bound to the focused control seen in the user's image.
use super::*;

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
                "Page.createIsolatedWorld",
                json!({
                    "frameId":frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false,
                }),
                deadline,
            )
            .await?;
        let value = cdp.call(Some(&frame.session), "Runtime.evaluate", json!({
            "contextId":context["executionContextId"],"expression":FOCUSED,"objectGroup":"rdh-human-focus",
        }), deadline).await?;
        let Some(object) = value["result"]["objectId"].as_str() else {
            continue;
        };
        let node = cdp
            .call(
                Some(&frame.session),
                "DOM.describeNode",
                json!({"objectId":object,"depth":0}),
                deadline,
            )
            .await;
        let _ = cdp
            .cleanup()
            .call(
                Some(&frame.session),
                "Runtime.releaseObjectGroup",
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
        let context = cdp.call(session,"Page.createIsolatedWorld",json!({
            "frameId":self.frame.id,"worldName":"rdh-browser-v1","grantUniveralAccess":false,
        }),deadline).await?;
        let node = cdp.call(session,"DOM.resolveNode",json!({"backendNodeId":self.backend,"executionContextId":context["executionContextId"],"objectGroup":"rdh-human-focus"}),deadline).await?;
        let object = node["object"]["objectId"]
            .as_str()
            .ok_or_else(|| Failure::new("browser_stale_frame"))?;
        let result = cdp
            .call(
                session,
                "Runtime.callFunctionOn",
                json!({"objectId":object,"functionDeclaration":INSERT,
            "arguments":[{"value":text}],"returnByValue":true}),
                deadline,
            )
            .await;
        let _ = cdp
            .cleanup()
            .call(
                session,
                "Runtime.releaseObjectGroup",
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
