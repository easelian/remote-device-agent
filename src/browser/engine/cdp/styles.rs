//! CSS domains own temporary frontend node IDs. Serialize their use per browser
//! connection and settle disable commands even if the waiting reader is gone.
use super::*;
use sem::styles::{StyleProperty, StyleScope, StyleSnapshot};
use tokio::sync::OwnedMutexGuard;

struct Cleanup {
    wire: Cdp,
    session: String,
    _gate: OwnedMutexGuard<()>,
    css_requested: bool,
}

impl Cleanup {
    async fn settle(self) -> Result<()> {
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        let wire = self.wire.cleanup();
        for command in [Command::CssDisable, Command::DomDisable] {
            if matches!(command, Command::CssDisable) && !self.css_requested {
                continue;
            }
            loop {
                if !wire.session_active(&self.session) {
                    return Ok(());
                }
                match wire
                    .call(Some(&self.session), command, json!({}), deadline)
                    .await
                {
                    Ok(_) => break,
                    Err(error)
                        if error.code == "browser_busy"
                            && error.state == "not_sent"
                            && Instant::now() < deadline =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    Err(_) if !wire.session_active(&self.session) => return Ok(()),
                    Err(_) => {
                        wire.stop();
                        return Err(Failure::unknown("browser_cleanup_pending"));
                    }
                }
            }
        }
        Ok(())
    }
}

struct Domains(Option<Cleanup>);
impl Domains {
    async fn finish(mut self) -> Result<()> {
        // The task owns the gate after spawn, even if the reader is cancelled
        // while it is awaiting the acknowledgment of CSS.disable or DOM.disable.
        tokio::spawn(self.0.take().unwrap().settle())
            .await
            .map_err(|_| Failure::unknown("browser_cleanup_pending"))?
    }
}
impl Drop for Domains {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            tokio::spawn(cleanup.settle());
        }
    }
}

const SAME_CHAIN: &str = r#"function (target) {
    if (!target.isConnected || target.getRootNode() !== document || target.assignedSlot) return false;
    let child = target;
    for (let i = 0; i < this.length; i++) {
        const parent = this[i];
        if (child.parentElement !== parent || (i < 4 && parent.assignedSlot)) return false;
        child = parent;
    }
    return this.length === 5 || !child.parentElement;
}"#;

async fn read_ancestors(
    wire: &Cdp,
    session: &str,
    object: &str,
    identity: &serde_json::Value,
    property: StyleProperty,
    deadline: Instant,
) -> Result<StyleSnapshot> {
    let call = Command::RuntimeCallFunction(Effect::None);
    let chain = wire
        .scoped_call(
            Some(session),
            call,
            json!({
                "objectId": object, "functionDeclaration": sem::evidence::SOURCE,
                "arguments": [{"value": "bind_style_ancestors"}, {"value": identity}],
                "objectGroup": "rdh-native-styles", "returnByValue": false
            }),
            deadline,
        )
        .await?;
    let result = chain.value();
    if result.get("exceptionDetails").is_some() {
        return Err(Failure::new("browser_script_exception"));
    }
    if result["result"]["subtype"] == "null" {
        return Err(Failure::new("browser_feature_unsupported"));
    }
    if result["result"]["subtype"] != "array" {
        return Err(sem::types::stale());
    }
    let chain_id = result["result"]["objectId"]
        .as_str()
        .ok_or_else(sem::types::decode)?;
    let length = wire
        .call(
            Some(session),
            call,
            json!({"objectId":chain_id,
                "functionDeclaration":"function () { return this.length; }", "returnByValue":true
            }),
            deadline,
        )
        .await?;
    let count = length["result"]["value"]
        .as_u64()
        .filter(|n| *n <= 5)
        .ok_or_else(sem::types::decode)? as u8;
    let examined = count.min(4);
    let mut projection = super::styles_project::Projection::new(property);
    let mut frontend_ids = Vec::with_capacity(usize::from(examined));
    for index in 0..examined {
        let parent = wire.scoped_call(Some(session), call, json!({
            "objectId":chain_id, "functionDeclaration":"function (index) { return this[index]; }",
            "arguments":[{"value":index}], "objectGroup":"rdh-native-styles", "returnByValue":false
        }), deadline).await?;
        let node = &parent.value()["result"];
        let parent_id = node["objectId"]
            .as_str()
            .filter(|_| node["subtype"] == "node")
            .ok_or_else(sem::types::stale)?;
        let frontend = wire
            .call(
                Some(session),
                Command::DomRequestNode,
                json!({"objectId":parent_id}),
                deadline,
            )
            .await?;
        let id = frontend["nodeId"]
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or_else(sem::types::stale)?;
        frontend_ids.push(id);
        let styles = wire
            .call(
                Some(session),
                Command::CssGetMatchedStylesForNode,
                json!({"nodeId":id}),
                deadline,
            )
            .await?;
        // Read declarations on the bound parent itself. Chromium's anonymous
        // inherited entries cannot establish the identity of a reparented node.
        projection.add_node(&styles, Some(index + 1))?;
    }
    // A light child assigned to a closed root can report assignedSlot == null.
    // The native host description includes even a closed root. Check after all
    // reads so a shadow root attached during CSS capture also retires the result.
    for id in frontend_ids {
        let described = wire
            .call(
                Some(session),
                Command::DomDescribeNode,
                json!({"nodeId":id,"depth":0,"pierce":false}),
                deadline,
            )
            .await?;
        let node = &described["node"];
        if node["nodeType"] != 1 || node["nodeId"] != id {
            return Err(sem::types::stale());
        }
        if let Some(roots) = node.get("shadowRoots") {
            if !roots.as_array().ok_or_else(sem::types::decode)?.is_empty() {
                return Err(Failure::new("browser_feature_unsupported"));
            }
        }
    }
    let check = wire.call(Some(session), call, json!({
        "objectId":chain_id, "functionDeclaration":SAME_CHAIN, "arguments":[{"objectId":object}],
        "returnByValue":true
    }), deadline).await?;
    if check["result"]["value"] != true {
        return Err(sem::types::stale());
    }
    Ok(projection.finish(Some(examined), count > examined))
}

pub(super) async fn read(
    owner: &Semantic,
    frame: &sem::Frame,
    node: u64,
    property: StyleProperty,
    scope: StyleScope,
    deadline: Instant,
) -> Result<StyleSnapshot> {
    let native = owner.native(frame)?;
    let world = owner.world(&native, deadline).await?;
    let session = owner.adapter.session(&native.session)?.to_owned();
    let gate = tokio::time::timeout_at(deadline.into(), owner.style_domains.clone().lock_owned())
        .await
        .map_err(|_| Failure::new("browser_deadline"))?;
    let wire = &owner.adapter.wire;
    let mut domains = Domains(Some(Cleanup {
        wire: wire.clone(),
        session: session.clone(),
        _gate: gate,
        css_requested: false,
    }));
    let result = async {
        wire.call(Some(&session), Command::DomEnable, json!({}), deadline)
            .await?;
        domains.0.as_mut().unwrap().css_requested = true;
        wire.call(Some(&session), Command::CssEnable, json!({}), deadline)
            .await?;
        let identity = json!({
            "realm": frame.marker.realm,
            "restoration": frame.marker.restoration,
            "node": node
        });
        let arguments = json!([{"value": identity.to_string()}]);
        let binding = wire
            .scoped_call(
                Some(&session),
                Command::RuntimeCallFunction(Effect::None),
                json!({
                    "executionContextId": world,
                    "functionDeclaration": super::accessibility::NODE,
                    "arguments": arguments,
                    "objectGroup": "rdh-native-styles",
                    "returnByValue": false
                }),
                deadline,
            )
            .await?;
        let value = binding.value();
        if value.get("exceptionDetails").is_some() {
            return Err(Failure::new("browser_script_exception"));
        }
        if value["result"]["subtype"] != "node" {
            return Err(sem::types::stale());
        }
        let object = value["result"]["objectId"]
            .as_str()
            .ok_or_else(sem::types::stale)?;
        // Seed the DOM agent before requesting a frontend ID for the bound wrapper.
        let document = wire
            .call(
                Some(&session),
                Command::DomGetDocument,
                json!({"depth": 0, "pierce": false}),
                deadline,
            )
            .await?;
        if document["root"]["nodeId"].as_i64().is_none_or(|id| id <= 0) {
            return Err(sem::types::stale());
        }
        let projected = match scope {
            StyleScope::Ancestors => {
                read_ancestors(wire, &session, object, &identity, property, deadline).await
            }
            StyleScope::Direct => {
                let frontend = wire
                    .call(
                        Some(&session),
                        Command::DomRequestNode,
                        json!({"objectId": object}),
                        deadline,
                    )
                    .await?;
                let id = frontend["nodeId"]
                    .as_i64()
                    .filter(|id| *id > 0)
                    .ok_or_else(sem::types::stale)?;
                wire
                    .call(
                        Some(&session),
                        Command::CssGetMatchedStylesForNode,
                        json!({"nodeId": id}),
                        deadline,
                    )
                    .await
                    .and_then(|matches| super::styles_project::project(&matches, property))
            }
        };
        // A native reply may arrive after DOM replacement. Compare the exact wrapper
        // against the registry in its original isolated realm before returning rules.
        let same_node = format!(
            "function (payload) {{ return this === ({})(payload); }}",
            super::accessibility::NODE
        );
        let check = wire
            .call(
                Some(&session),
                Command::RuntimeCallFunction(Effect::None),
                json!({
                    "objectId": object, "functionDeclaration": same_node,
                    "arguments": arguments, "returnByValue": true
                }),
                deadline,
            )
            .await?;
        if check.get("exceptionDetails").is_some() || check["result"]["value"] != true {
            return Err(sem::types::stale());
        }
        projected
    }
    .await;
    domains.finish().await?;
    result
}
