use super::{
    Result,
    cdp::{Cdp, evaluate},
};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Element {
    pub session: String,
    pub backend: i64,
    loader: String,
}

pub async fn loader(cdp: &Cdp, session: &str) -> Result<String> {
    Ok(cdp
        .call(Some(session), "Page.getFrameTree", json!({}))
        .await?["frameTree"]["frame"]["loaderId"]
        .as_str()
        .ok_or("missing_document_identity")?
        .into())
}

pub async fn observe(cdp: &Cdp, session: &str) -> Result<Value> {
    let before = loader(cdp, session).await?;
    let tree = cdp
        .call(Some(session), "Accessibility.getFullAXTree", json!({}))
        .await?;
    let nodes: Vec<_> = tree["nodes"]
        .as_array()
        .ok_or("missing_ax_nodes")?
        .iter()
        .filter(|n| n["ignored"] != true)
        .take(1000)
        .map(|n| {
            json!({"id":n["nodeId"],"role":n["role"]["value"],
            "name":n["name"]["value"],"children":n["childIds"],"backend":n["backendDOMNodeId"]})
        })
        .collect();
    if before != loader(cdp, session).await? {
        return Err("navigation_during_observation".into());
    }
    let result = json!({"loader":before,"nodes":nodes,"prototype_scope":"one_target_only"});
    if serde_json::to_vec(&result)?.len() > 256 * 1024 {
        return Err("observation_too_large".into());
    }
    Ok(result)
}

pub async fn find(cdp: &Cdp, session: &str, name: &str, role: &str) -> Result<Element> {
    let observation = observe(cdp, session).await?;
    let matches: Vec<_> = observation["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["name"] == name && n["role"] == role)
        .collect();
    if matches.len() != 1 {
        return Err("target_not_unique".into());
    }
    Ok(Element {
        session: session.into(),
        backend: matches[0]["backend"].as_i64().ok_or("no_dom_identity")?,
        loader: observation["loader"].as_str().unwrap().into(),
    })
}

impl Element {
    pub async fn check(&self, cdp: &Cdp) -> Result<Value> {
        if self.loader != loader(cdp, &self.session).await? {
            return Err("stale_document".into());
        }
        let object = cdp
            .call(
                Some(&self.session),
                "DOM.resolveNode",
                json!({"backendNodeId":self.backend}),
            )
            .await?["object"]["objectId"]
            .as_str()
            .ok_or("stale_element")?
            .to_owned();
        let result = cdp.call(Some(&self.session), "Runtime.callFunctionOn", json!({"objectId":object,
            "functionDeclaration":"function(){if(!this.isConnected)return {connected:false};const r=this.getBoundingClientRect();const x=r.x+r.width/2,y=r.y+r.height/2;const hit=this.getRootNode().elementFromPoint(x,y);return {connected:true,enabled:!this.disabled,visible:r.width>0&&r.height>0,hit:hit===this||this.contains(hit),x,y}}",
            "returnByValue":true})).await;
        let _ = cdp
            .call(
                Some(&self.session),
                "Runtime.releaseObject",
                json!({"objectId":object}),
            )
            .await;
        let result = result?["result"]["value"].clone();
        if result["connected"] != true {
            return Err("stale_element".into());
        }
        if result["enabled"] != true || result["visible"] != true || result["hit"] != true {
            return Err("not_actionable".into());
        }
        Ok(result)
    }

    pub async fn click(&self, cdp: &Cdp) -> Result<()> {
        let point = self.check(cdp).await?;
        for kind in ["mousePressed", "mouseReleased"] {
            cdp.call(
                Some(&self.session),
                "Input.dispatchMouseEvent",
                json!({"type":kind,
                "x":point["x"],"y":point["y"],"button":"left","clickCount":1}),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn fill(&self, cdp: &Cdp, text: &str) -> Result<()> {
        self.check(cdp).await?;
        cdp.call(
            Some(&self.session),
            "DOM.focus",
            json!({"backendNodeId":self.backend}),
        )
        .await?;
        // Explicit select-all then browser text insertion; no direct assignment to value.
        cdp.call(Some(&self.session), "Input.dispatchKeyEvent", json!({"type":"keyDown","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65})).await?;
        cdp.call(Some(&self.session), "Input.dispatchKeyEvent", json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65})).await?;
        cdp.call(
            Some(&self.session),
            "Input.insertText",
            json!({"text":text}),
        )
        .await?;
        Ok(())
    }
}

pub async fn navigate(cdp: &Cdp, session: &str, url: &str) -> Result<()> {
    let url = url::Url::parse(url)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("navigation_scheme_denied".into());
    }
    cdp.call(Some(session), "Page.navigate", json!({"url":url.as_str()}))
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if evaluate(cdp, session, "document.readyState === 'complete'").await? == true {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}
