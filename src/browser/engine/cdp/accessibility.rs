//! Bounded native AX projection. Native identities never cross this adapter.
use super::*;
use sem::{
    accessibility::{NameStatus, Snapshot, Status},
    evidence::{Coverage, Limit, Omitted},
};

pub(super) const NODE: &str = r#"function(serialized) {
  const a=JSON.parse(serialized),c=globalThis.__rdhSemanticDomV1,s=globalThis.__rdhDomEvidenceV1;
  if(!c||!s||c.document!==document||s.document!==document||c.realm!==a.realm||
     s.realm!==a.realm||c.restoration!==a.restoration||s.restoration!==a.restoration)return null;
  const entry=s.nodes.get(a.node);
  if(!entry)return null;
  if(performance.now()-entry.created>=60000){s.nodes.delete(a.node);return null;}
  const node=entry.weak.deref();
  return node&&node.isConnected&&node.ownerDocument===document?node:null;
}"#;
const BOOLEANS: &[&str] = &[
    "atomic",
    "busy",
    "disabled",
    "focusable",
    "focused",
    "hidden",
    "modal",
    "multiline",
    "multiselectable",
    "readonly",
    "required",
    "selected",
    "expanded",
];
const IGNORED: &[&str] = &[
    "activeFullscreenElement",
    "activeModalDialog",
    "activeAriaModalDialog",
    "ariaHiddenElement",
    "ariaHiddenSubtree",
    "emptyAlt",
    "emptyText",
    "inertElement",
    "inertSubtree",
    "labelContainer",
    "labelFor",
    "notRendered",
    "notVisible",
    "presentationalRole",
    "probablyPresentational",
    "inactiveCarouselTabContent",
    "uninteresting",
];
fn short(value: &str, limit: usize, reason: Limit, coverage: &mut Coverage) -> String {
    let mut chars = value.chars();
    let result = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        coverage.limit(reason);
    }
    result
}
fn property(name: &str, value: &Value) -> Option<Value> {
    let kind = value["type"].as_str()?;
    let v = &value["value"];
    if BOOLEANS.contains(&name) && matches!(kind, "boolean" | "booleanOrUndefined") {
        return v.as_bool().map(Value::Bool);
    }
    if matches!(name, "checked" | "pressed")
        && matches!(kind, "tristate" | "boolean" | "booleanOrUndefined")
    {
        return v.as_bool().map(Value::Bool).or_else(|| match v.as_str()? {
            "true" => Some(true.into()),
            "false" => Some(false.into()),
            "mixed" => Some("mixed".into()),
            _ => None,
        });
    }
    if name == "invalid"
        && kind == "token"
        && matches!(v.as_str(), Some("false" | "true" | "grammar" | "spelling"))
    {
        return Some(v.clone());
    }
    if name == "editable" && kind == "token" && matches!(v.as_str(), Some("plaintext" | "richtext"))
    {
        return Some(v.clone());
    }
    if name == "level" && matches!(kind, "integer" | "number") {
        return v.as_u64().filter(|n| *n <= 1024).map(Value::from);
    }
    None
}
fn array<'a>(value: &'a Value, name: &str) -> Result<&'a [Value]> {
    match value.get(name) {
        None => Ok(&[]),
        Some(v) => v
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(sem::types::decode),
    }
}
fn project(
    value: &Value,
    facts: &sem::evidence::Node,
    coverage: &mut Coverage,
) -> Result<Snapshot> {
    coverage.omitted.extend([
        Omitted::NativeAxTree,
        Omitted::NativeAxValue,
        Omitted::NativeAxDescription,
        Omitted::NativeAxRelations,
        Omitted::NativeAxOtherProperties,
    ]);
    let nodes = value["nodes"]
        .as_array()
        .filter(|n| n.len() <= 1)
        .ok_or_else(sem::types::decode)?;
    coverage.native_ax_nodes = Some(nodes.len());
    coverage.native_ax_properties = Some(0);
    coverage.native_ax_ignored_reasons = Some(0);
    let Some(node) = nodes.first() else {
        return Ok(Snapshot::absent());
    };
    if node["nodeId"]
        .as_str()
        .is_none_or(|id| id.is_empty() || id.len() > 256)
    {
        return Err(sem::types::decode());
    }
    let ignored = node["ignored"].as_bool().ok_or_else(sem::types::decode)?;
    let mut result = Snapshot::absent();
    result.status = Status::Present;
    result.ignored = Some(ignored);
    if node["role"]["type"] == "role"
        && let Some(role) = node["role"]["value"].as_str()
    {
        result.role = Some(short(role, 64, Limit::AxRole, coverage));
    }
    if matches!(
        node["name"]["type"].as_str(),
        Some("string" | "computedString")
    ) && let Some(name) = node["name"]["value"].as_str()
    {
        let name = short(name, 256, Limit::AxName, coverage);
        if !facts.sensitive && name == facts.name {
            result.name = Some(name);
            result.name_status = NameStatus::Available;
        } else {
            result.name_status = NameStatus::Omitted;
            coverage.omitted.insert(Omitted::NativeAxName);
        }
    }
    let properties = array(node, "properties")?;
    coverage.native_ax_properties = Some(properties.len().min(32));
    if properties.len() > 32 {
        coverage.limit(Limit::AxProperty);
    }
    for item in properties.iter().take(32) {
        if let Some(name) = item["name"].as_str()
            && let Some(value) = property(name, &item["value"])
        {
            if result.states.insert(name.into(), value).is_some() {
                return Err(sem::types::decode());
            }
        } else {
            coverage.omitted.insert(Omitted::UnrecognizedAxProperty);
        }
    }
    let reasons = array(node, "ignoredReasons")?;
    coverage.native_ax_ignored_reasons = Some(reasons.len().min(16));
    if reasons.len() > 16 {
        coverage.limit(Limit::AxReason);
    }
    for reason in reasons.iter().take(16) {
        if let Some(name) = reason["name"]
            .as_str()
            .filter(|name| IGNORED.contains(name))
        {
            result.ignored_reasons.insert(name.into());
        } else {
            coverage.omitted.insert(Omitted::UnrecognizedAxReason);
        }
    }
    Ok(result)
}
impl Semantic {
    pub(super) async fn read_accessibility(
        &self,
        frame: &sem::Frame,
        serial: u64,
        facts: &sem::evidence::Node,
        coverage: &mut Coverage,
        deadline: Instant,
    ) -> Result<Snapshot> {
        let native = self.native(frame)?;
        let world = self.world(&native, deadline).await?;
        let session = Some(self.adapter.session(&native.session)?);
        // The returned scope owns the wrapper even after its creation reply
        // was consumed. Cancellation of the later AX call still drops it.
        let binding=self.adapter.wire.scoped_call(session,Command::RuntimeCallFunction(Effect::None),json!({"executionContextId":world,"functionDeclaration":NODE,"arguments":[{"value":json!({"realm":frame.marker.realm,"restoration":frame.marker.restoration,"node":serial}).to_string()}],"objectGroup":"rdh-native-ax","returnByValue":false}),deadline).await?;
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
        let value = self
            .adapter
            .wire
            .call(
                session,
                Command::AccessibilityGetPartialAXTree,
                json!({"objectId":object,"fetchRelatives":false}),
                deadline,
            )
            .await?;
        project(&value, facts, coverage)
    }
    #[cfg(test)]
    pub fn hold_accessibility_reply(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.adapter
            .wire
            .hold_next_reply_for(Command::AccessibilityGetPartialAXTree)
    }
    #[cfg(test)]
    pub fn hold_styles_reply(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.adapter
            .wire
            .hold_next_reply_for(Command::CssGetMatchedStylesForNode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn facts(name: &str, sensitive: bool) -> sem::evidence::Node {
        sem::evidence::Node {
            node: 1,
            tag: "button".into(),
            role: "button".into(),
            name: name.into(),
            name_source: sem::evidence::NameSource::DirectText,
            sensitive,
        }
    }
    fn sample() -> Value {
        json!({"nodes":[{"nodeId":"private-native-id","ignored":false,"role":{"type":"role","value":"button"},"name":{"type":"computedString","value":"Allowed"},"value":{"value":"NEVER-DISCLOSE"},"properties":[{"name":"focusable","value":{"type":"boolean","value":true}},{"name":"checked","value":{"type":"tristate","value":"mixed"}}]}]})
    }
    #[test]
    fn native_ax_projection_never_discloses_unapproved_names_values_or_native_ids() {
        let mut coverage = Coverage::base();
        let value = project(&sample(), &facts("Allowed", false), &mut coverage).unwrap();
        let value = serde_json::to_value(value).unwrap();
        assert_eq!(value["name"], "Allowed");
        assert_eq!(value["states"]["focusable"], true);
        assert_eq!(value["states"]["checked"], "mixed");
        assert!(!value.to_string().contains("private-native-id"));
        assert!(!value.to_string().contains("NEVER-DISCLOSE"));
        assert_eq!(coverage.native_ax_nodes, Some(1));
        for f in [facts("Other", false), facts("Allowed", true)] {
            let value =
                serde_json::to_value(project(&sample(), &f, &mut Coverage::base()).unwrap())
                    .unwrap();
            assert_eq!(value["name_status"], "omitted");
            assert!(value["name"].is_null());
        }
    }
    #[test]
    fn native_ax_projection_bounds_fields_and_refuses_a_tree() {
        let mut v = sample();
        let second = v["nodes"][0].clone();
        v["nodes"].as_array_mut().unwrap().push(second);
        assert!(project(&v, &facts("Allowed", false), &mut Coverage::base()).is_err());
        let mut v = sample();
        v["nodes"][0]["name"]["value"] = "😀".repeat(300).into();
        v["nodes"][0]["role"]["value"] = "x".repeat(100).into();
        v["nodes"][0]["properties"] = json!(vec![
            json!({"name":"unsupported","value":{"type":"string","value":"SECRET"}});
            33
        ]);
        v["nodes"][0]["ignoredReasons"] = json!(vec![
            json!({"name":"notVisible","value":{"value":"SECRET"}});
            17
        ]);
        let mut coverage = Coverage::base();
        let value = project(&v, &facts(&"😀".repeat(256), false), &mut coverage).unwrap();
        assert_eq!(value.name.unwrap().chars().count(), 256);
        assert_eq!(value.role.unwrap().len(), 64);
        assert!(value.states.is_empty());
        assert_eq!(value.ignored_reasons.len(), 1);
        assert_eq!(coverage.native_ax_properties, Some(32));
        assert_eq!(coverage.native_ax_ignored_reasons, Some(16));
        assert_eq!(
            coverage.limits,
            [
                Limit::AxName,
                Limit::AxRole,
                Limit::AxProperty,
                Limit::AxReason
            ]
            .into()
        );
        let absent = serde_json::to_value(
            project(
                &json!({"nodes":[]}),
                &facts("", false),
                &mut Coverage::base(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(absent["status"], "absent");
        assert!(absent["ignored"].is_null());
    }
}
