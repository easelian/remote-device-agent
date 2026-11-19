use super::overview::build;
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

// Every fixture has the same material root, including across documents.
fn row(frame: &Frame, id: u64, role: &str, name: &str, headings: &[u64]) -> Row {
    let entry = |identity, tag: &str| Entry {
        identity,
        tag: tag.into(),
        attributes: Vec::new(),
        text: String::new(),
        value: None,
        checked: false,
    };
    let mut entries = vec![entry(1, "article")];
    entries.extend(headings.iter().map(|id| entry(*id, "h2")));
    Row {
        context: frame.context,
        reference: None,
        facts: Facts {
            node: id,
            id: String::new(),
            role: role.into(),
            kind: "native".into(),
            name: name.into(),
            name_source: "content".into(),
            name_quality: Vec::new(),
            value: None,
            enabled: true,
            readonly: false,
            editable: false,
            sensitive: false,
            checked: false,
            tag: if role == "heading" { "h2" } else { "button" }.into(),
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

fn view(rows: &[Row], frames: &[Frame], incomplete: bool, budget: usize) -> Value {
    let nodes: Vec<_> = rows
        .iter()
        .map(|row| {
            let index = frames
                .iter()
                .position(|frame| frame.context == row.context)
                .unwrap();
            json!({"frame_id": format!("frame-{index}")})
        })
        .collect();
    build(rows, &nodes, frames, incomplete, budget)
}

#[test]
fn record_requires_a_single_observed_heading_in_the_same_frame_and_material() {
    let mut ids = Identities::new();
    let page = frame(&mut ids, "page", None);
    let child = frame(&mut ids, "child", Some(page.context));
    let rows = [
        row(&page, 2, "heading", "Page record", &[2]),
        row(&page, 3, "button", "Edit", &[2]),
        row(&child, 2, "heading", "Child record", &[2]),
        row(&child, 3, "button", "Edit", &[2]),
        row(&child, 4, "button", "No heading", &[]),
        row(&child, 5, "button", "Unobserved heading", &[7]),
        row(&child, 6, "button", "Ambiguous", &[2, 7]),
    ];
    let result = view(&rows, &[page, child], false, 20_000);
    let frames = result["frames"].as_array().unwrap();
    assert_eq!(frames[0]["scope"], "iframe");
    let items = frames[0]["items"].as_array().unwrap();
    assert_eq!(items[1]["record"], "Child record");
    assert!(items[2..].iter().all(|item| item.get("record").is_none()));
    assert_eq!(frames[1]["items"][1]["record"], "Page record");
    assert_eq!(result["truncated"], false);
}

#[test]
fn sensitive_rows_and_overlong_record_identifiers_do_not_escape_into_the_view() {
    let page = frame(&mut Identities::new(), "page", None);
    let mut secret = row(&page, 7, "heading", "Secret", &[7]);
    secret.facts.sensitive = true;
    let rows = [
        row(&page, 2, "heading", &"界".repeat(161), &[2]),
        row(&page, 3, "button", "Long heading", &[2]),
        row(&page, 4, "button", "Private heading", &[7]),
        secret,
    ];
    let result = view(&rows, &[page], false, 20_000);
    let items = result["frames"][0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["name"].as_str().unwrap().chars().count(), 160);
    assert_eq!(items[0]["name_truncated"], true);
    assert!(items.iter().all(|item| item.get("record").is_none()));
    assert_eq!(result["truncated"], true);
}

#[test]
fn iframe_items_take_priority_within_item_and_character_limits() {
    let mut ids = Identities::new();
    let page = frame(&mut ids, "page", None);
    let child = frame(&mut ids, "child", Some(page.context));
    let mut rows = vec![row(&page, 2, "button", "Next", &[])];
    rows.extend((2..22).map(|id| row(&child, id, "button", "Save", &[])));
    let frames = [page, child];
    let full = view(&rows, &frames, false, 20_000);
    assert_eq!(full["frames"][0]["items"].as_array().unwrap().len(), 16);
    assert_eq!(full["truncated"], true);
    let small = view(&rows, &frames, false, 128);
    assert_eq!(small["frames"][0]["scope"], "iframe");
    assert_eq!(small["truncated"], true);
    assert!(small.to_string().len() <= 128);
    assert_eq!(view(&rows[..1], &frames, true, 20_000)["truncated"], true);
}
