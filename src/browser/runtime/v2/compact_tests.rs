use super::compact::{reference, write};
use crate::browser::semantic::{Context, Entry, Facts, Frame, Identities, Marker, Material, Row};
use serde_json::{Value, json};

fn frame(ids: &mut Identities, key: &str, parent: Option<Context>) -> Frame {
    ids.observe(
        key,
        parent,
        &format!("{key}-document"),
        &format!("{key}-realm"),
        Marker {
            realm: format!("{key}-realm"),
            restoration: 1,
            url: format!("https://example.test/{key}"),
        },
    )
    .unwrap()
}
fn element(identity: u64, tag: &str) -> Entry {
    Entry {
        identity,
        tag: tag.into(),
        attributes: Vec::new(),
        text: String::new(),
        value: None,
        checked: false,
    }
}
fn text(identity: u64, text: &str) -> Entry {
    Entry {
        text: text.into(),
        ..element(identity, "#text")
    }
}
fn row(frame: &Frame, node: u64, role: &str, name: &str, entries: Vec<Entry>) -> Row {
    Row {
        context: frame.context,
        reference: None,
        facts: Facts {
            node,
            id: String::new(),
            role: role.into(),
            kind: if role == "text" { "text" } else { "native" }.into(),
            name: name.into(),
            name_source: "content".into(),
            name_quality: Vec::new(),
            value: None,
            enabled: true,
            readonly: false,
            editable: false,
            sensitive: false,
            checked: false,
            tag: "div".into(),
            input_type: String::new(),
            visible: true,
            hit: true,
            x: 1.0,
            y: 1.0,
            in_view: true,
            record: Material {
                owner_list: None,
                owner_context: Vec::new(),
                entries,
                complete: true,
                limits: Vec::new(),
            },
        },
    }
}
fn order(root: u64, label: &str) -> Vec<Entry> {
    vec![
        element(root, "tr"),
        element(root + 1, "td"),
        text(root + 2, label),
        element(root + 3, "button"),
        text(root + 4, "Approve"),
    ]
}
fn project(rows: &[Row], full: &[Value], frames: &[Frame]) -> Value {
    let mut value = json!({});
    write(&mut value, rows, full, frames);
    value
}

#[test]
fn a_record_shared_by_several_nodes_is_written_once_and_defaults_are_left_out() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let rows = [
        row(
            &top,
            1,
            "heading",
            "Orders",
            vec![element(9, "h1"), text(10, "Orders")],
        ),
        row(&top, 2, "text", "Order B42", order(20, "Order B42")),
        row(&top, 3, "button", "Approve", order(20, "Order B42")),
        row(&top, 4, "button", "Approve", order(30, "Order B43")),
    ];
    let full = [
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f","ref":"r1"}),
        json!({"frame_id":"f","ref":"r2"}),
    ];
    let value = project(&rows, &full, &[top]);
    assert_eq!(
        value,
        json!({"format":"compact_v1",
            "records":[
                {"entries":[["tr"],"Order B42","Approve"]},
                {"entries":[["tr"],"Order B43","Approve"]}],
            "frames":[{"frame_id":"f","scope":"page","nodes":[
                {"role":"heading","name":"Orders"},
                {"role":"button","name":"Approve","ref":"r1","record":0},
                {"role":"button","name":"Approve","ref":"r2","record":1}]}]})
    );
}

#[test]
fn a_node_keeps_every_fact_that_differs_from_the_ordinary_one() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let mut field = row(&top, 1, "textbox", "Email", vec![element(5, "input")]);
    field.facts.value = Some("a@example.test".into());
    field.facts.record.entries[0].value = Some("a@example.test".into());
    field.facts.editable = true;
    field.facts.name_source = "label".into();
    field.facts.input_type = "email".into();
    field.facts.in_view = false;
    let mut agree = row(&top, 2, "checkbox", "Agree", vec![element(6, "input")]);
    agree.facts.enabled = false;
    agree.facts.in_view = false;
    let mut option = row(&top, 3, "option", "Blue", vec![element(7, "div")]);
    option.facts.kind = "aria".into();
    option.facts.record.entries[0].attributes = vec![("aria-posinset".into(), "3".into())];
    option.facts.record.complete = false;
    option.facts.record.limits = vec!["record_node_limit".into()];
    let mut secret = row(&top, 4, "textbox", "Secret", vec![element(8, "input")]);
    secret.facts.sensitive = true;
    secret.facts.input_type = "password".into();
    let full = [
        json!({"frame_id":"f","ref":"r1"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
    ];
    let value = project(&[field, agree, option, secret], &full, &[top]);
    assert_eq!(
        value["frames"][0]["nodes"],
        json!([
            {"role":"textbox","name":"Email","ref":"r1","offscreen":true,"value":"a@example.test",
                "editable":true,"name_source":"label","input_type":"email"},
            {"role":"checkbox","name":"Agree","disabled":true,"checked":false},
            {"role":"option","name":"Blue","record":0,"kind":"aria"},
            {"role":"textbox","name":"Secret","sensitive":true,"input_type":"password"}])
    );
    assert_eq!(
        value["records"],
        json!([{"entries":[["div",{"attributes":{"aria-posinset":"3"}}]],
            "complete":false,"limits":["record_node_limit"]}])
    );
    assert!(!value.to_string().contains("a@example.test\",\"value"));
}

#[test]
fn nodes_stay_in_reading_order_under_their_own_frame_and_documents_share_no_record() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let child = frame(&mut ids, "child", Some(top.context));
    let rows = [
        row(&top, 1, "button", "Approve", order(20, "Order B42")),
        row(&child, 1, "button", "Approve", order(20, "Order B42")),
        row(&top, 2, "text", "After", vec![text(40, "After")]),
    ];
    let full = [
        json!({"frame_id":"top"}),
        json!({"frame_id":"child"}),
        json!({"frame_id":"top"}),
    ];
    let value = project(&rows, &full, &[top, child]);
    let frames = value["frames"].as_array().unwrap();
    assert_eq!(
        frames
            .iter()
            .map(|f| (
                f["frame_id"].as_str().unwrap(),
                f["scope"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [("top", "page"), ("child", "iframe"), ("top", "page")]
    );
    assert_eq!(value["records"].as_array().unwrap().len(), 2);
    assert_eq!(frames[1]["nodes"][0]["record"], 1);
    assert_eq!(project(&[], &[], &[])["frames"], json!([]));
}

#[test]
fn text_already_listed_by_a_record_is_not_repeated_beside_its_control() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let cells = |root| {
        vec![
            element(root, "tr"),
            text(root + 1, "Fruit"),
            text(root + 2, "Pear"),
        ]
    };
    let mut cut = row(&top, 5, "text", "Long", order(20, "Order B42"));
    cut.facts.name_quality = vec!["text_character_limit".into()];
    let rows = [
        row(&top, 1, "text", "Order B42", order(20, "Order B42")),
        row(&top, 2, "button", "Approve", order(20, "Order B42")),
        row(&top, 3, "text", "Fruit", cells(40)),
        row(&top, 4, "text", "Pear", cells(40)),
        cut,
        row(&top, 6, "text", "Elsewhere", order(20, "Order B42")),
    ];
    let full = vec![json!({"frame_id":"f"}); rows.len()];
    let value = project(&rows, &full, &[top]);
    assert_eq!(
        value["frames"][0]["nodes"],
        json!([
            {"role":"button","name":"Approve","record":0},
            {"name":"Fruit","record":1},
            {"name":"Long","record":0,"name_quality":["text_character_limit"]},
            {"name":"Elsewhere","record":0}])
    );
    assert_eq!(
        value["records"][1],
        json!({"entries":[["tr"],"Fruit","Pear"]})
    );
}

#[test]
fn a_short_reference_is_never_issued_twice_and_fits_the_public_pattern() {
    let mut issued = (String::new(), 0);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..5000 {
        let id = reference(&mut issued).unwrap();
        assert!(
            id.len() <= 10 && id.bytes().all(|b| b.is_ascii_alphanumeric()),
            "{id}"
        );
        assert!(id.starts_with(&format!("r{}", issued.0)));
        assert!(seen.insert(id));
    }
}

#[test]
fn the_compact_format_is_closed_and_separate_from_delta_presentation() {
    let validate = super::super::validate;
    let plain = json!({"session_id":"session","control_epoch":1,"format":"compact"});
    assert!(validate("browser_observe", &plain).is_ok());
    let mut continued = plain.clone();
    continued["cursor"] = "0".repeat(32).into();
    assert!(validate("browser_observe", &continued).is_ok());
    for format in [json!("full"), json!(true), Value::Null] {
        let mut request = plain.clone();
        request["format"] = format;
        assert!(validate("browser_observe", &request).is_err(), "{request}");
    }
    let mut request = plain.clone();
    request["presentation"] = json!({"version":1,"mode":"delta"});
    assert!(validate("browser_observe", &request).is_err());
    assert!(
        validate(
            "browser_act",
            &json!({"session_id":"s","control_epoch":1,"format":"compact"})
        )
        .is_err()
    );
}
