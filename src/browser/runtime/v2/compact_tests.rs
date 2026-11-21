use super::compact::{reference, trim, write};
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
                same: None,
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
fn lines(value: &Value) -> Vec<&str> {
    value["text"].as_str().unwrap().lines().collect()
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
    assert_eq!(value["format"], "compact_v1");
    assert_eq!(
        lines(&value),
        [
            "heading: Orders",
            "<tr> Order B42",
            "  button [r1]: Approve",
            "<tr> Order B43",
            "  button [r2]: Approve"
        ]
    );
    assert_eq!(value.as_object().unwrap().len(), 2);
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
    option.facts.record.entries[0].attributes = vec![
        ("role".into(), "option".into()),
        ("aria-posinset".into(), "3".into()),
    ];
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
        lines(&value),
        [
            "textbox [r1] offscreen value=a@example.test type=email: Email",
            "checkbox disabled unchecked: Agree",
            "option kind=aria aria-posinset=3: Blue",
            "textbox sensitive type=password: Secret"
        ]
    );
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
    // Only a child frame is headed, and the page is named again after it.
    assert_eq!(
        lines(&value),
        [
            "<tr> Order B42",
            "  button: Approve",
            "--- iframe child",
            "<tr> Order B42",
            "  button: Approve",
            "--- page",
            "text: After"
        ]
    );
    assert_eq!(project(&[], &[], &[])["text"], "");
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
    // A record with no control stands for its text. One met again further
    // down is written as its first element and an ellipsis.
    assert_eq!(
        lines(&value),
        [
            "<tr> Order B42",
            "  button: Approve",
            "<tr> Fruit | Pear",
            "<tr> …",
            "  text quality=text_character_limit: Long",
            "  text: Elsewhere"
        ]
    );
}

#[test]
fn a_record_does_not_restate_its_node_and_page_text_stays_on_its_line() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let input = |attributes: &[(&str, &str)]| {
        let mut input = element(5, "input");
        input.attributes = attributes
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        input.value = Some("on".into());
        input.checked = true;
        let mut node = row(&top, 1, "checkbox", "Show advanced", vec![input]);
        node.facts.value = Some("on".into());
        node.facts.checked = true;
        node.facts.readonly = true;
        node.facts.input_type = "checkbox".into();
        node.facts.name_source = "aria-label".into();
        node
    };
    let restated = [
        ("aria-label", "Show advanced"),
        ("type", "checkbox"),
        ("readonly", ""),
    ];
    let mut more = restated.to_vec();
    more.extend([("id", "adv"), ("title", "Shows more")]);
    let mut odd = row(
        &top,
        2,
        "text",
        "a\\b\nc: d",
        vec![element(7, "li"), text(8, "x < y | z"), text(9, "tail")],
    );
    odd.facts.name_quality = vec!["text_character_limit".into()];
    let full = vec![json!({"frame_id":"f"}); 3];
    let value = project(&[input(&restated), input(&more), odd], &full, &[top]);
    let node = "checkbox readonly checked: Show advanced";
    assert_eq!(
        lines(&value),
        [
            node,
            node,
            "<li> x \\< y \\| z | tail",
            "  text quality=text_character_limit: a\\\\b\\nc: d"
        ]
    );
}

#[test]
fn text_named_by_a_control_leaves_the_record_with_its_bare_element() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let mut link = element(4, "a");
    link.attributes = vec![("href".into(), "/k1".into())];
    let card = vec![
        element(1, "article"),
        element(2, "h3"),
        text(3, "Order K1"),
        link,
        text(5, "Open"),
        text(6, "Pending"),
    ];
    let rows = [
        row(&top, 1, "heading", "Order K1", card.clone()),
        row(&top, 2, "link", "Open", card),
    ];
    let full = vec![json!({"frame_id":"f"}); 2];
    assert_eq!(
        lines(&project(&rows, &full, &[top])),
        [
            "<article> <a href=/k1> | Pending",
            "  heading: Order K1",
            "  link: Open"
        ]
    );
}

#[test]
fn what_another_line_says_is_left_out_and_addresses_only_part_twins() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let with = |mut entry: Entry, attributes: &[(&str, &str)], value: Option<&str>| {
        entry.attributes = attributes
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        entry.value = value.map(Into::into);
        entry
    };
    let label = vec![
        with(element(10, "label"), &[("for", "plan")], None),
        text(11, "Plan"),
    ];
    let mut plan = row(
        &top,
        12,
        "combobox",
        "Plan",
        vec![
            with(element(12, "select"), &[("id", "plan")], Some("free")),
            with(element(13, "option"), &[], Some("free")),
            text(14, "Free"),
            with(element(15, "option"), &[], Some("pro")),
            text(16, "Pro"),
        ],
    );
    plan.facts.tag = "select".into();
    plan.facts.value = Some("free".into());
    plan.facts.name_source = "label".into();
    plan.facts.input_type = "select-one".into();
    let mut dialog = row(
        &top,
        20,
        "unsupported",
        "Confirm",
        vec![
            with(
                element(20, "div"),
                &[("role", "dialog"), ("aria-label", "Confirm")],
                None,
            ),
            text(21, "Delete order B42?"),
            element(22, "button"),
            text(23, "Cancel"),
        ],
    );
    dialog.facts.kind = "aria".into();
    dialog.facts.name_source = "aria-label".into();
    dialog.facts.name_quality = vec!["unsupported_role".into()];
    let button = |node, name: &str, id| {
        row(
            &top,
            node,
            "button",
            name,
            vec![
                with(element(node, "button"), &[("id", id)], None),
                text(node + 1, name),
            ],
        )
    };
    let status = || {
        vec![
            with(element(60, "p"), &[("role", "status")], None),
            text(61, "Deleted B42"),
        ]
    };
    let mut holder = row(&top, 60, "unsupported", "", status());
    holder.facts.kind = "aria".into();
    let rows = [
        holder,
        row(&top, 61, "text", "Deleted B42", status()),
        row(&top, 1, "text", "Plan", label),
        plan,
        dialog,
        row(
            &top,
            21,
            "text",
            "Delete order B42?",
            vec![text(21, "Delete order B42?")],
        ),
        button(22, "Cancel", "no"),
        button(30, "Remove", "first"),
        button(40, "Remove", "second"),
        button(50, "Next", "next"),
    ];
    let full = [
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f","ref":"r1"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f","ref":"r2"}),
        json!({"frame_id":"f","ref":"r3"}),
        json!({"frame_id":"f","ref":"r4"}),
        json!({"frame_id":"f","ref":"r5"}),
    ];
    // A container outside every record is a line of its own. The label is the
    // name of its control, a container's content is listed by the nodes
    // inside it, and an id is written only for controls that read alike.
    assert_eq!(
        lines(&project(&rows, &full, &[top])),
        [
            "status kind=aria",
            "text: Deleted B42",
            "<select> <option value=free> Free <option value=pro> Pro",
            "  combobox [r1] value=free: Plan",
            "dialog kind=aria: Confirm",
            "text: Delete order B42?",
            "button [r2]: Cancel",
            "button [r3] id=first: Remove",
            "button [r4] id=second: Remove",
            "button [r5]: Next"
        ]
    );
}

#[test]
fn a_node_outside_every_record_is_one_line_with_its_own_attributes() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let with = |mut entry: Entry, attributes: &[(&str, &str)]| {
        entry.attributes = attributes
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        entry
    };
    // Text of a paragraph, as the page program sends it: its parent and the
    // text, with the pieces of one block already joined.
    let piece = |node, words: &str| {
        row(
            &top,
            node,
            "text",
            words,
            vec![element(100, "p"), text(node, "Type a")],
        )
    };
    let link = |node, name: &str, attributes: &[(&str, &str)]| {
        row(
            &top,
            node,
            "link",
            name,
            vec![
                with(element(node, "a"), attributes),
                element(node + 1, "span"),
                text(node + 2, name),
            ],
        )
    };
    let mut card = row(
        &top,
        40,
        "inferred",
        "Open order",
        vec![
            with(element(40, "div"), &[("aria-expanded", "false")]),
            text(41, "Open order"),
        ],
    );
    card.facts.kind = "inferred".into();
    let mut item = row(
        &top,
        50,
        "unsupported",
        "",
        vec![
            with(element(50, "div"), &[("role", "listitem")]),
            text(51, "Order 7"),
        ],
    );
    item.facts.kind = "aria".into();
    let rows = [
        piece(1, "Type a URL into the"),
        link(
            10,
            "address bar",
            &[
                ("href", "https://example.test/wiki/Address_bar"),
                ("title", "Address bar"),
            ],
        ),
        piece(5, "and press Enter."),
        link(20, "More", &[("href", "https://other.test/a?b=1")]),
        link(30, "More", &[("href", "#notes"), ("target", "_blank")]),
        card,
        item,
    ];
    let mut full = vec![json!({"frame_id":"f"}); rows.len()];
    for (at, reference) in [(1, "r1"), (3, "r2"), (4, "r3"), (5, "r4")] {
        full[at]["ref"] = reference.into();
    }
    // A link into the page's own site is written from its path on, a tooltip
    // beside a name is left out, and only a record keeps a line of its own.
    assert_eq!(
        lines(&project(&rows, &full, &[top])),
        [
            "text: Type a URL into the",
            "link [r1] href=/wiki/Address_bar: address bar",
            "text: and press Enter.",
            "link [r2] href=\"https://other.test/a?b=1\": More",
            "link [r3] href=#notes target=_blank: More",
            "inferred [r4] tag=div aria-expanded=false: Open order",
            "<div role=listitem> Order 7"
        ]
    );
}

#[test]
fn a_record_cut_short_is_not_written_as_a_record() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let mut post = element(1, "article");
    post.attributes = vec![("aria-label".into(), "Post by Ada".into())];
    let cut = |node, role: &str, name: &str, entries: Vec<Entry>| {
        let mut row = row(&top, node, role, name, entries);
        row.facts.record.complete = false;
        row.facts.record.limits = vec!["record_node_limit".into()];
        row
    };
    let whole = || vec![post.clone(), element(2, "h2"), text(3, "Title")];
    let mut first = cut(10, "button", "Reply", whole());
    first.facts.id = "a".into();
    let mut second = cut(11, "button", "Reply", whole());
    second.facts.id = "b".into();
    let rows = [
        cut(2, "heading", "Title", whole()),
        // Text of such a record carries only the record's own element.
        cut(4, "text", "function f() {}", vec![post.clone()]),
        first,
        row(&top, 20, "link", "Elsewhere", vec![element(20, "a")]),
        second,
    ];
    let mut full = vec![json!({"frame_id":"f"}); rows.len()];
    for (at, reference) in [(2, "r1"), (3, "r2"), (4, "r3")] {
        full[at]["ref"] = reference.into();
    }
    // The element is written once, for what it says of itself, and its
    // nodes are neither indented nor headed again after other content.
    assert_eq!(
        lines(&project(&rows, &full, &[top])),
        [
            "<article aria-label=\"Post by Ada\">",
            "heading: Title",
            "text: function f() {}",
            "button [r1] id=a: Reply",
            "link [r2]: Elsewhere",
            "button [r3] id=b: Reply"
        ]
    );
}

#[test]
fn what_holds_for_every_reply_is_left_out() {
    let mut ids = Identities::new();
    let top = frame(&mut ids, "top", None);
    let item = |root| vec![element(root, "li"), text(root + 1, "Order B42")];
    let holder = |name: &str| {
        let mut holder = row(&top, 1, "unsupported", name, item(20));
        holder.facts.kind = "aria".into();
        holder.facts.name_quality = vec!["unsupported_role".into()];
        holder
    };
    let mut submit = row(&top, 4, "button", "Save", item(20));
    submit.facts.input_type = "submit".into();
    let rows = [
        holder(""),
        holder("Orders"),
        holder(""),
        row(&top, 3, "text", "Order B42", item(20)),
        submit,
    ];
    let full = [
        json!({"frame_id":"f"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f","ref":"r1"}),
        json!({"frame_id":"f"}),
        json!({"frame_id":"f","ref":"r2"}),
    ];
    let value = project(&rows, &full, &[top]);
    // The unnamed container without a reference is gone; a named or referenced one stays.
    assert_eq!(
        lines(&value),
        [
            "<li> Order B42",
            "  unsupported kind=aria quality=unsupported_role: Orders",
            "  unsupported [r1] kind=aria quality=unsupported_role",
            "  button [r2]: Save"
        ]
    );

    let mut reply = json!({"page":{"url":"u","scroll_x":0.0,"scroll_y":12.5},
        "coverage":{"complete":true,"changed_during_read":false,"scanned":3,"omitted":[
            "closed_shadow_not_observable","canvas_pixels_outside_profile","browser_ui_outside_profile"]}});
    trim(&mut reply);
    assert_eq!(
        reply,
        json!({"page":{"url":"u","scroll_y":12.5},"coverage":{"complete":true,"scanned":3}})
    );
    let mut reply = json!({"coverage":{"complete":false,"changed_during_read":true,"scanned":3,
        "omitted":["browser_ui_outside_profile","node_limit"]}});
    trim(&mut reply);
    assert_eq!(
        reply["coverage"],
        json!({"complete":false,"changed_during_read":true,"scanned":3,"omitted":["node_limit"]})
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
