use super::*;

pub(super) trait Harness: Port {
    fn clear(&self) -> impl Future<Output = Result<()>> + Send;
    fn evidence(&self, id: &str) -> impl Future<Output = Result<Value>> + Send;
}
impl Harness for engine::semantic_cdp::Semantic {
    async fn clear(&self) -> Result<()> {
        self.fixture("(events=[],true)", deadline()).await.map(drop)
    }
    async fn evidence(&self, id: &str) -> Result<Value> {
        self.fixture(&expression(id), deadline()).await
    }
}
impl Harness for engine::bidi::page::Semantic {
    async fn clear(&self) -> Result<()> {
        self.fixture("(events=[],true)", deadline()).await.map(drop)
    }
    async fn evidence(&self, id: &str) -> Result<Value> {
        self.fixture(&expression(id), deadline()).await
    }
}
fn expression(id: &str) -> String {
    format!(
        "(()=>{{const e=document.getElementById({}),s=getSelection(),empty=!e.firstChild||(e.childNodes.length===1&&e.firstChild.nodeName==='BR');return {{value:e.isContentEditable?(empty?'':e.innerText.replace(/\\r\\n/g,'\\n')):e.value,raw_inner_text:e.innerText,html:e.innerHTML,active:document.activeElement.id,selection:s?{{anchor:s.anchorNode?.parentElement?.id,focus:s.focusNode?.parentElement?.id,text:s.toString()}}:null,events,url:location.href}};}})()",
        serde_json::to_string(id).unwrap()
    )
}
impl Harness for Fake {
    async fn clear(&self) -> Result<()> {
        self.data.lock().unwrap().events.clear();
        Ok(())
    }
    async fn evidence(&self, id: &str) -> Result<Value> {
        let data = self.data.lock().unwrap();
        let row = data.rows.iter().find(|row| row.id == id).unwrap();
        Ok(json!({"value":row.value,"events":data.events,"url":data.frame.url}))
    }
}
async fn reference<P: Port>(page: &Page<P>, id: &str) -> Reference {
    let read = page
        .observe(
            Query {
                filter: Filter::Interactive,
                ..Default::default()
            },
            200,
            64000,
            None,
            deadline(),
        )
        .await
        .unwrap();
    read.rows
        .iter()
        .find(|row| row.facts.id == id)
        .unwrap_or_else(|| panic!("missing {id}"))
        .reference
        .clone()
        .unwrap_or_else(|| panic!("no reference for {id}"))
}
pub(super) async fn assert_inputs<P: Harness>(port: P, label: &str) -> Value {
    let page = Page::new(port.clone());
    let mut results = Vec::new();
    for id in ["label", "multiline", "editable"] {
        for value in &oracle().replace_text {
            let target = reference(&page, id).await;
            let before = port.evidence(id).await.unwrap();
            port.clear().await.unwrap();
            let result = page
                .act(target, Action::ReplaceText(value.clone()), deadline())
                .await;
            let evidence = port.evidence(id).await.unwrap();
            if let Some(root) = std::env::var_os("RDH_D3_EVIDENCE") {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(PathBuf::from(root).join(format!("{label}-input-trace.jsonl")))
                    .unwrap();
                writeln!(file,"{}",json!({"id":id,"expected":value,"result":result.as_ref().map(|a|json!(a)).unwrap_or_else(|e|json!({"code":e.code,"state":e.state})),"evidence":evidence})).unwrap();
            }
            if id == "label" && value.contains('\n') {
                let failure = result.unwrap_err();
                assert_eq!(failure.code, "browser_invalid_params");
                assert_eq!(failure.state, "not_sent");
                assert_eq!(evidence["value"], before["value"]);
                assert!(evidence["events"].as_array().unwrap().is_empty());
                results.push(json!({"id":id,"value":value,"rejected":"single_line","events":evidence["events"]}));
                continue;
            }
            let ack = result.unwrap_or_else(|e| panic!("input {id} {value:?}: {e:?}"));
            assert!(ack.input_acknowledged);
            assert!(!ack.business_result_verified);
            assert_eq!(ack.path, "native_edit");
            assert_eq!(evidence["value"], value.as_str(), "value for {id}");
            assert_eq!(evidence["url"], before["url"]);
            let events = evidence["events"].as_array().unwrap();
            assert!(!events.iter().any(|event| event["type"] == "submit"));
            if ack.changed {
                assert!(
                    events.iter().any(|event| event["type"] == "input"
                        && event["id"] == id
                        && event["trusted"] == true),
                    "missing native input: {evidence}"
                );
                assert!(
                    events
                        .iter()
                        .filter(|event| event["type"] == "input")
                        .all(|event| event["id"] == id)
                );
            }
            results.push(json!({"id":id,"value":value,"ack":ack,"events":events}));
            let current = reference(&page, id).await;
            port.clear().await.unwrap();
            page.act(current, Action::Press(Key::ArrowRight), deadline())
                .await
                .unwrap();
            let evidence = port.evidence(id).await.unwrap();
            let events = evidence["events"].as_array().unwrap();
            assert!(
                events
                    .iter()
                    .any(|event| event["type"] == "keydown" && event["key"] == "ArrowRight")
            );
            assert!(
                events
                    .iter()
                    .filter(|event| event["type"] == "keydown")
                    .all(|event| event["ctrl"] == false && event["meta"] == false),
                "residual modifier: {evidence}"
            );
            page.settled(deadline()).await.unwrap();
            assert_eq!(page.resources().1, 0);
        }
    }
    // Same final value is an idempotent intent and must not generate input.
    let current = reference(&page, "editable").await;
    port.clear().await.unwrap();
    let value = oracle().replace_text.last().unwrap().clone();
    assert!(
        !page
            .act(current, Action::ReplaceText(value), deadline())
            .await
            .unwrap()
            .changed
    );
    assert!(
        port.evidence("editable").await.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    json!({"cases":results,"residual_modifiers":0,"extra_submits":0,"idempotent_no_input":true})
}
#[tokio::test]
async fn fake_uses_the_native_input_intent_assertions() {
    assert_inputs(Fake::new(), "fake").await;
}
