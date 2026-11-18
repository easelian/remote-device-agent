use super::*;
fn frame() -> sem::Frame {
    sem::Identities::new()
        .observe(
            "main",
            None,
            "document",
            "realm",
            sem::Marker {
                realm: "stamp".into(),
                restoration: 0,
                url: "https://example.test/".into(),
            },
        )
        .unwrap()
}
#[test]
fn evidence_requests_are_closed_and_diagnostic_ids_are_not_refs() {
    for query in [
        json!({"kind":"find_nodes","tag":"div"}),
        json!({"kind":"find_nodes","role":"region","text":"状态","scope":"all_frames","limit":64}),
        json!({"kind":"read_node","node_id":format!("den_{}","a".repeat(32))}),
        json!({"kind":"read_accessibility","node_id":format!("den_{}","a".repeat(32))}),
    ] {
        assert!(Query::parse(&query).is_ok());
    }
    for query in [
        json!({"kind":"find_nodes"}),
        json!({"kind":"find_nodes","tag":null,"role":"div"}),
        json!({"kind":"find_nodes","tag":"DIV"}),
        json!({"kind":"find_nodes","tag":"div","limit":65}),
        json!({"kind":"find_nodes","text":"😀".repeat(129)}),
        json!({"kind":"find_nodes","tag":"div","selector":"*"}),
        json!({"kind":"read_node","node_id":"ref_123"}),
        json!({"kind":"read_accessibility","node_id":format!("den_{}","a".repeat(32)),"depth":1}),
        json!({"kind":"read_layout","node_id":format!("den_{}","g".repeat(32))}),
    ] {
        assert!(Query::parse(&query).is_err(), "{query}");
    }
}
#[test]
fn ancestor_style_parser_rejects_display_but_accepts_safe_properties() {
    let node_id = format!("den_{}", "a".repeat(32));
    assert!(
        Query::parse(
            &json!({"kind":"read_ancestor_styles","node_id":node_id,"property":"display"})
        )
        .is_err()
    );
    assert!(
        Query::parse(
            &json!({"kind":"read_ancestor_styles","node_id":node_id,"property":"visibility"})
        )
        .is_ok()
    );
}

#[test]
fn evidence_identity_capacity_scope_expiration_and_no_renewal() {
    let frame = frame();
    let target = dom::target(std::slice::from_ref(&frame), &frame, 1).unwrap();
    let mut table = Registry::default();
    let mut ids = Vec::new();
    for _ in 0..=MAX_ENTRIES {
        let (id, entry) = entry(target, "tab", 7).unwrap();
        ids.push(id);
        table.publish(Prepared {
            entries: vec![entry],
        });
    }
    assert_eq!(table.entries.len(), MAX_ENTRIES);
    assert!(table.entries.capacity() * std::mem::size_of::<Entry>() <= MAX_ALLOCATION);
    assert!(table.lookup(&ids[0], "tab", 7).is_err());
    let last = ids.last().unwrap();
    let created = table.entries.last().unwrap().created;
    assert_eq!(table.lookup(last, "tab", 7).unwrap().node, 1);
    assert_eq!(table.entries.last().unwrap().created, created);
    assert!(table.lookup(last, "other", 7).is_err());
    assert!(table.lookup(last, "tab", 8).is_err());
    assert!(Registry::default().lookup(last, "tab", 7).is_err());
    table.expire(created + TTL);
    assert!(table.entries.is_empty());
}
#[test]
fn evidence_identity_digest_covers_realm_document_url_and_ancestry() {
    let frame = frame();
    let original = dom::target(std::slice::from_ref(&frame), &frame, 1).unwrap();
    for changed in 0..4 {
        let mut next = frame.clone();
        match changed {
            0 => next.document += 1,
            1 => next.realm += 1,
            2 => next.marker.restoration += 1,
            _ => next.url.push_str("other"),
        };
        assert_ne!(
            original.digest,
            dom::target(std::slice::from_ref(&next), &next, 1)
                .unwrap()
                .digest
        );
    }
}
#[test]
fn evidence_decoder_rejects_extra_content_and_false_coverage() {
    let request = dom::NativeQuery {
        anchor_text: None,
        expected_text: None,
        kind: "find_nodes",
        tag: Some("div".into()),
        role: None,
        text: None,
        node: None,
        limit: 16,
        elements: 5,
        text_nodes: 10,
        other_nodes: 10,
    };
    let base = json!({"kind":"find_nodes","nodes":[{"node":1,"tag":"div","role":"","name":"test","name_source":"direct_text","sensitive":false}],"coverage":{"visited_elements":5,"visited_text_nodes":1,"visited_other_nodes":0,"truncated":false,"limits":[],"omitted":[]}});
    assert!(dom::Raw::decode(&base.to_string(), &request).is_ok());
    let mut wrong = base.clone();
    wrong["nodes"][0]["value"] = "private".into();
    assert!(dom::Raw::decode(&wrong.to_string(), &request).is_err());
    wrong = base.clone();
    wrong["coverage"]["visited_elements"] = 6.into();
    assert!(dom::Raw::decode(&wrong.to_string(), &request).is_err());
    wrong = base.clone();
    wrong["coverage"]["limits"] = json!(["element_limit"]);
    assert!(dom::Raw::decode(&wrong.to_string(), &request).is_err());
    wrong = base.clone();
    wrong["coverage"]["native_ax_nodes"] = 1.into();
    assert!(dom::Raw::decode(&wrong.to_string(), &request).is_err());
    wrong = base;
    wrong["nodes"][0]["name"] = "😀".repeat(257).into();
    assert!(dom::Raw::decode(&wrong.to_string(), &request).is_err());
}
#[test]
fn record_matching_is_internal_and_requires_complete_coverage() {
    let binding = format!("den_{}", "a".repeat(32));
    let public = json!({"kind":"match_record","node_id":binding,
        "anchor_text":"Order-42","expected_text":"Approved"});
    assert!(dom::Query::parse(&public).is_err());
    let request = dom::NativeQuery {
        kind: "match_record", tag: None, role: None, text: None, node: Some(1),
        limit: 1, elements: 256, text_nodes: 256, other_nodes: 256,
        anchor_text: Some("Order-42".into()), expected_text: Some("Approved".into()),
    };
    let mut result = json!({"kind":"match_record","matched":true,"coverage":{
        "visited_elements":2,"visited_text_nodes":2,"visited_other_nodes":0,
        "truncated":false,"limits":[],"omitted":[]}});
    assert!(dom::Raw::decode(&result.to_string(), &request).is_ok());
    result["coverage"]["truncated"] = true.into();
    result["coverage"]["limits"] = json!(["element_limit"]);
    assert!(dom::Raw::decode(&result.to_string(), &request).is_err());
}

#[test]
fn validation_reads_the_requested_operation_and_only_discloses_fixed_reasons() {
    let query = dom::NativeQuery {
        anchor_text: None,
        expected_text: None,
        kind: "read_validation",
        node: Some(1),
        tag: None,
        role: None,
        text: None,
        limit: 1,
        elements: 8,
        other_nodes: 8,
        text_nodes: 8,
    };
    let program = sem::program::Program::Evidence {
        marker: frame().marker,
        query: query.clone(),
    };
    assert_eq!(program.encode().0, "read_validation");
    let mut raw = json!({"kind":"read_validation","validation":{"state":"invalid","reasons":["value_missing"]},
        "coverage":{"visited_elements":1,"visited_text_nodes":0,"visited_other_nodes":0,"truncated":false,"limits":[],"omitted":[]}});
    assert!(dom::Raw::decode(&raw.to_string(), &query).is_ok());
    for bad in [
        json!({"state":"valid","reasons":["custom_error"]}),
        json!({"state":"invalid","reasons":["value_missing","value_missing"]}),
        json!({"state":"invalid","reasons":["private-site-message"]}),
        json!({"state":"omitted","reasons":[],"message":"secret"}),
    ] {
        raw["validation"] = bad;
        assert!(dom::Raw::decode(&raw.to_string(), &query).is_err());
    }
}

#[test]
fn evidence_output_byte_limit_drops_matching_unpublished_bindings() {
    let frame = frame();
    let target = dom::target(std::slice::from_ref(&frame), &frame, 1).unwrap();
    let mut value =
        json!({"kind":"find_nodes","coverage":{"limits":[],"truncated":false},"nodes":[]});
    let mut prepared = Prepared {
        entries: Vec::new(),
    };
    for _ in 0..64 {
        let (id, entry) = entry(target, "tab", 1).unwrap();
        value["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"node_id":id,"name":"😀".repeat(256)}));
        prepared.entries.push(entry);
    }
    bounded(&mut value, &mut prepared).unwrap();
    assert!(serde_json::to_vec(&value).unwrap().len() <= dom::MAX_BYTES);
    assert_eq!(
        value["nodes"].as_array().unwrap().len(),
        prepared.entries.len()
    );
    assert_eq!(value["coverage"]["truncated"], true);
    assert_eq!(value["coverage"]["limits"], json!(["byte_limit"]));
}
#[path = "evidence_native_tests.rs"]
mod native_tests;
