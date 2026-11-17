use super::*;

fn frames() -> HashMap<String, sem::Context> {
    let mut identities = sem::types::Identities::new();
    HashMap::from([
        ("frame-a".into(), identities.context("private-a").unwrap()),
        ("frame-b".into(), identities.context("private-b").unwrap()),
    ])
}

fn observation(names: &[&str], generation: usize) -> Value {
    let nodes: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut row = json!({"name":name,"role":"button","kind":"native",
                "enabled":true,"sensitive":false,"frame_id":if index%2==0{"frame-a"}else{"frame-b"},
                "record_complete":true,"record":{"entries":[],"complete":true,"limits":[]}});
            if index % 3 != 0 {
                row["ref"] = format!("{:032x}", generation * 1000 + index).into();
            }
            row
        })
        .collect();
    json!({"observation_profile":"semantic_dom_v1","content_trust":"untrusted_page",
        "nodes":nodes,"page":{"tab_id":"tab","title":"fixture","url":"https://fixture.test/"},
        "coverage":{"complete":true,"changed_during_read":false,"omitted":[],"scanned":names.len()}})
}

fn prepare(
    cache: &mut Cache,
    frames: &HashMap<String, sem::Context>,
    value: &mut Value,
    base: Option<&str>,
    now: Instant,
) -> Prepared {
    cache.prepare(value, frames, [7; 32], base, now).unwrap()
}

// The independent receiver oracle reconstructs the ordinary public observation.
// It deliberately does not use the encoded cache or its prefix/suffix helpers.
fn ordinary(mut response: Value) -> Value {
    let mut presentation = response
        .as_object_mut()
        .unwrap()
        .remove("presentation")
        .unwrap();
    let mut nodes = presentation["nodes"].as_array_mut().unwrap().clone();
    for (index, node) in nodes.iter_mut().enumerate() {
        let object = node.as_object_mut().unwrap();
        let slot = object.remove("frame_slot").unwrap().as_u64().unwrap() as usize;
        object.insert(
            "frame_id".into(),
            presentation["bindings"]["frames"][slot].clone(),
        );
        if !presentation["bindings"]["references"][index].is_null() {
            object.insert(
                "ref".into(),
                presentation["bindings"]["references"][index].clone(),
            );
        }
    }
    response["nodes"] = nodes.into();
    if let Some(cursor) = presentation["bindings"].get("next_cursor") {
        response["next_cursor"] = cursor.clone();
    }
    response
}

#[test]
fn full_projection_preserves_canonical_fields_and_rotates_every_binding() {
    let frames = frames();
    let mut cache = Cache::default();
    let now = Instant::now();
    let mut first = observation(&["\u{a0}é", "e\u{301}", "🦀", "readonly"], 1);
    first["nodes"][0]["record"]["entries"] = json!([
        {"tag":"td","attributes":[["data-id","α"]],"text":"🦀","value":null,"checked":false}
    ]);
    let expected = first.clone();
    let prepared = prepare(&mut cache, &frames, &mut first, None, now);
    assert_eq!(ordinary(first.clone()), expected);
    assert!(cache.baselines.is_empty(), "preparation is not publication");
    cache.publish(prepared).unwrap();
    let previous = first["presentation"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut second = expected.clone();
    second["nodes"][1]["ref"] = "ffffffffffffffffffffffffffffffff".into();
    let expected_second = second.clone();
    let prepared = prepare(&mut cache, &frames, &mut second, Some(&previous), now);
    assert_eq!(second["presentation"]["_candidate"]["unchanged"], true);
    assert_ne!(
        first["presentation"]["bindings"],
        second["presentation"]["bindings"]
    );
    assert_eq!(ordinary(second), expected_second);
    cache.publish(prepared).unwrap();
    for entry in &cache.baselines {
        let stored = std::str::from_utf8(&entry.encoded.bytes).unwrap();
        assert!(!stored.contains("ffffffffffffffffffffffffffffffff"));
        assert!(!stored.contains("frame-a") && !stored.contains("frame-b"));
    }
}

#[test]
fn splice_reconstructs_insert_delete_reorder_metadata_and_form_changes() {
    let frames = frames();
    let now = Instant::now();
    let mut cache = Cache::default();
    let mut original = observation(&["one", "two", "three", "four"], 1);
    let prepared = prepare(&mut cache, &frames, &mut original, None, now);
    cache.publish(prepared).unwrap();
    let revision = original["presentation"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    for names in [
        vec!["one", "two", "three", "four"],
        vec!["one", "two", "changed", "four"],
        vec!["one", "new", "two", "three", "four"],
        vec!["one", "three", "four"],
        vec!["four", "three", "two", "one"],
        vec![],
    ] {
        let mut fresh = observation(&names, 2);
        if names.len() == 4 && names[0] == "one" {
            fresh["nodes"][1]["checked"] = true.into();
            fresh["nodes"][1]["value"] = "new value".into();
            fresh["nodes"][1]["name_quality"] = json!(["explicit_label"]);
        }
        fresh["page"]["title"] = "current metadata".into();
        let oracle = fresh.clone();
        let prepared = prepare(&mut cache, &frames, &mut fresh, Some(&revision), now);
        let hint = &fresh["presentation"]["_candidate"];
        let start = hint["start"].as_u64().unwrap() as usize;
        let remove = hint["delete_count"].as_u64().unwrap() as usize;
        let insert = hint["insert_count"].as_u64().unwrap() as usize;
        let mut reconstructed = original["presentation"]["nodes"]
            .as_array()
            .unwrap()
            .clone();
        let current = fresh["presentation"]["nodes"].as_array().unwrap();
        reconstructed.splice(
            start..start + remove,
            current[start..start + insert].iter().cloned(),
        );
        assert_eq!(&reconstructed, current);
        assert_eq!(ordinary(fresh), oracle);
        drop(prepared); // A lost result is never assumed to have been applied.
    }
    let mut metadata_only = observation(&["one", "two", "three", "four"], 3);
    metadata_only["page"]["title"] = "metadata changed".into();
    prepare(
        &mut cache,
        &frames,
        &mut metadata_only,
        Some(&revision),
        now,
    );
    assert_eq!(
        metadata_only["presentation"]["_candidate"]["insert_count"],
        0
    );
    assert_eq!(
        metadata_only["presentation"]["_candidate"]["unchanged"],
        false
    );
}

#[test]
fn eviction_expiry_and_foreign_revisions_never_keep_implicit_client_state() {
    let frames = frames();
    let now = Instant::now();
    let mut cache = Cache::default();
    let mut revisions = Vec::new();
    for index in 0..3 {
        let mut value = observation(&["one", "two"], index);
        let prepared = prepare(
            &mut cache,
            &frames,
            &mut value,
            revisions.last().map(String::as_str),
            now,
        );
        revisions.push(
            value["presentation"]["revision"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        cache.publish(prepared).unwrap();
    }
    assert_eq!(cache.baselines.len(), 2);
    let mut value = observation(&["one", "two"], 4);
    prepare(&mut cache, &frames, &mut value, Some(&revisions[0]), now);
    assert!(value["presentation"].get("_candidate").is_none());
    let mut value = observation(&["one", "two"], 5);
    prepare(
        &mut cache,
        &frames,
        &mut value,
        Some("other-session-revision"),
        now,
    );
    assert!(value["presentation"].get("_candidate").is_none());
    let mut value = observation(&["one", "two"], 6);
    prepare(
        &mut cache,
        &frames,
        &mut value,
        Some(&revisions[2]),
        now + TTL - Duration::from_nanos(1),
    );
    assert!(value["presentation"].get("_candidate").is_some());
    let mut value = observation(&["one", "two"], 7);
    prepare(
        &mut cache,
        &frames,
        &mut value,
        Some(&revisions[2]),
        now + TTL,
    );
    assert!(value["presentation"].get("_candidate").is_none());
    assert!(
        cache.baselines.is_empty(),
        "a cache hit does not renew the old version"
    );
}

#[test]
fn restricted_observations_keep_fresh_full_data_without_retaining_a_baseline() {
    let frames = frames();
    let now = Instant::now();
    for restriction in 0..5 {
        let mut cache = Cache::default();
        let mut first = observation(&["one", "two"], 1);
        let prepared = prepare(&mut cache, &frames, &mut first, None, now);
        cache.publish(prepared).unwrap();
        let base = first["presentation"]["revision"].as_str().unwrap();
        let mut next = observation(&["one", "two"], 2);
        match restriction {
            0 => next["coverage"]["complete"] = false.into(),
            1 => next["coverage"]["changed_during_read"] = true.into(),
            2 => next["next_cursor"] = "0123456789abcdef0123456789abcdef".into(),
            3 => next["nodes"][0]["record"]["limits"] = json!(["record_bytes"]),
            4 => next["nodes"][0]["record_complete"] = false.into(),
            _ => unreachable!(),
        }
        let oracle = next.clone();
        let prepared = prepare(&mut cache, &frames, &mut next, Some(base), now);
        assert_eq!(next["presentation"]["cacheable"], false);
        assert!(next["presentation"].get("_candidate").is_none());
        assert_eq!(ordinary(next), oracle);
        cache.publish(prepared).unwrap();
        assert_eq!(cache.metrics()["encoded_bytes"], 0);
    }
}

#[test]
fn encoded_boundary_is_utf8_exact_and_oversize_never_allocates_a_baseline() {
    let mut value = observation(&[], 0);
    let initial = encode(&value, 0).unwrap().unwrap().bytes.len();
    value["page"]["padding"] = "".into();
    let overhead = encode(&value, 0).unwrap().unwrap().bytes.len();
    assert!(overhead > initial);
    value["page"]["padding"] = "x".repeat(MAX_BASELINE - overhead).into();
    let exact = encode(&value, 0).unwrap().unwrap();
    assert_eq!(exact.bytes.len(), MAX_BASELINE);
    assert_eq!(exact.bytes.capacity(), MAX_BASELINE);
    value["page"]["padding"] = "x".repeat(MAX_BASELINE - overhead + 1).into();
    assert!(encode(&value, 0).unwrap().is_none());
    value["page"]["padding"] = ("x".repeat(MAX_BASELINE - overhead - 4) + "🦀").into();
    assert_eq!(
        encode(&value, 0).unwrap().unwrap().bytes.len(),
        MAX_BASELINE
    );
    value["page"]["padding"] = ("x".repeat(MAX_BASELINE - overhead - 3) + "🦀").into();
    assert!(encode(&value, 0).unwrap().is_none());
}

#[test]
fn cancellation_key_changes_and_sequence_exhaustion_do_not_publish_stale_work() {
    let frames = frames();
    let now = Instant::now();
    let mut cache = Cache::default();
    let mut first = observation(&["one", "two"], 1);
    let abandoned = prepare(&mut cache, &frames, &mut first, None, now);
    let mut second = observation(&["one", "two"], 2);
    let current = cache
        .prepare(&mut second, &frames, [8; 32], None, now)
        .unwrap();
    assert!(cache.publish(abandoned).is_err());
    cache.publish(current).unwrap();
    let mut third = observation(&["one", "two"], 3);
    let prepared = cache
        .prepare(&mut third, &frames, [8; 32], None, now)
        .unwrap();
    cache.clear();
    assert!(cache.publish(prepared).is_err());
    assert_eq!(cache.metrics()["entries"], 0);
    cache.sequence = u64::MAX;
    let mut value = observation(&["one"], 4);
    assert!(
        cache
            .prepare(&mut value, &frames, [8; 32], None, now)
            .is_err()
    );
    assert_eq!(cache.sequence, u64::MAX);
    assert!(value.get("presentation").is_none());
}

#[test]
fn ten_thousand_replacements_keep_encoded_and_index_storage_bounded() {
    let frames = frames();
    let now = Instant::now();
    let mut cache = Cache::default();
    let mut last = None;
    for index in 0..10_000 {
        let mut value = observation(&["one", "two", "three", "four"], index);
        let prepared = prepare(&mut cache, &frames, &mut value, last.as_deref(), now);
        last = value["presentation"]["revision"]
            .as_str()
            .map(str::to_owned);
        cache.publish(prepared).unwrap();
        assert!(cache.baselines.len() <= 2);
        if index % 17 == 0 {
            cache.clear();
        }
    }
    let stats = cache.metrics();
    assert!(stats["encoded_bytes"].as_u64().unwrap() <= (2 * MAX_BASELINE) as u64);
    assert!(stats["node_indices"].as_u64().unwrap() <= 2_000);
    assert!(stats["frame_slots"].as_u64().unwrap() <= 64);
    assert!(stats["construction_peak"].as_u64().unwrap() <= MAX_CONSTRUCTION as u64);
    cache.clear();
    assert_eq!(cache.metrics()["encoded_bytes"], 0);
    assert_eq!(cache.metrics()["node_indices"], 0);
    assert_eq!(cache.metrics()["frame_slots"], 0);
}

#[test]
fn verified_identity_query_and_control_changes_invalidate_even_at_the_same_url() {
    let mut identities = sem::types::Identities::new();
    let frame = identities
        .observe(
            "private-frame",
            None,
            "doc",
            "realm",
            sem::types::Marker {
                realm: "marker".into(),
                restoration: 0,
                url: "https://fixture.test/".into(),
            },
        )
        .unwrap();
    let query = sem::Query::default();
    let original = key(std::slice::from_ref(&frame), &query, "tab", 1, 200, 20_000).unwrap();
    for part in ["document", "realm", "restoration"] {
        let mut changed = frame.clone();
        match part {
            "document" => changed.document += 1,
            "realm" => changed.realm += 1,
            "restoration" => changed.marker.restoration += 1,
            _ => unreachable!(),
        }
        assert_ne!(
            original,
            key(&[changed], &query, "tab", 1, 200, 20_000).unwrap()
        );
    }
    for (tab, epoch, nodes, characters) in [
        ("other", 1, 200, 20_000),
        ("tab", 2, 200, 20_000),
        ("tab", 1, 201, 20_000),
        ("tab", 1, 200, 20_001),
    ] {
        assert_ne!(
            original,
            key(
                std::slice::from_ref(&frame),
                &query,
                tab,
                epoch,
                nodes,
                characters
            )
            .unwrap()
        );
    }
    let scoped = sem::Query {
        within: Some(frame.context),
        ..query
    };
    assert_ne!(
        original,
        key(&[frame], &scoped, "tab", 1, 200, 20_000).unwrap()
    );
}

#[path = "presentation_native_tests.rs"]
mod native;

#[path = "presentation_allocations.rs"]
mod allocations;

#[test]
fn actual_constructor_allocations_and_maximum_encoded_cache_capacity_are_bounded() {
    let frames = frames();
    let now = Instant::now();
    let mut sizing = Cache::default();
    // This codec-level fixture exercises all 1,000 index slots independently
    // of the earlier native character and canonical-record budgets.
    let names = vec!["x"; 1000];
    let mut sample = observation(&names, 0);
    sample["page"]["padding"] = "".into();
    let prepared = prepare(&mut sizing, &frames, &mut sample, None, now);
    let encoded = prepared.baseline.as_ref().unwrap().encoded.bytes.len();
    let mut caches = [Cache::default(), Cache::default()];
    let mut maximum = 0;
    for (session, cache) in caches.iter_mut().enumerate() {
        for generation in 1..=3 {
            let mut value = observation(&names, session * 10 + generation);
            value["page"]["padding"] = "p".repeat(MAX_BASELINE - encoded).into();
            let (prepared, allocated) =
                allocations::measure(|| prepare(cache, &frames, &mut value, None, now));
            maximum = maximum.max(allocated);
            assert_eq!(
                prepared.baseline.as_ref().unwrap().encoded.bytes.len(),
                MAX_BASELINE
            );
            assert!(
                allocated <= MAX_CONSTRUCTION,
                "constructor allocated {allocated} bytes, limit {MAX_CONSTRUCTION}"
            );
            cache.publish(prepared).unwrap();
        }
    }
    assert_eq!(
        caches
            .iter()
            .map(|cache| cache.metrics()["allocated_bytes"].as_u64().unwrap())
            .sum::<u64>(),
        1024 * 1024
    );
    assert_eq!(
        caches
            .iter()
            .map(|cache| cache.metrics()["node_indices"].as_u64().unwrap())
            .sum::<u64>(),
        4000
    );
    println!(
        "display allocation upper bound: {maximum}; stored bytes: 1048576; node indices: 4000"
    );
    for cache in &mut caches {
        cache.clear();
        assert_eq!(cache.metrics()["allocated_bytes"], 0);
    }
}
