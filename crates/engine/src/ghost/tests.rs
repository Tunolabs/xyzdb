// SPDX-License-Identifier: BUSL-1.1
use super::*;
use xyzdb_core::value::Value;

/// DROP of a lightweight ghost must purge its rollup namespace from
/// the dictionary keyspace — orphan rollups would be silent disk
/// growth on every CREATE/REFRESH cycle. In-crate test because the
/// dictionary handle is pub(crate). The spill limit is forced tiny;
/// the value matches every other user of the knob in this crate so
/// intra-process env races stay benign.
#[test]
fn drop_purges_lightweight_rollups() {
    // SAFETY: same value as all other setters of this knob.
    unsafe { std::env::set_var("XYZ_GHOST_SUMMARIES_MAX_GROUPS", "4") };
    let dir = tempfile::tempdir().unwrap();
    let engine = crate::engine::Engine::open(dir.path()).unwrap();
    let run = |s: &str| {
        engine
            .run(s)
            .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
    };
    run(r#"LOBE "creditos""#);
    for g in 0..10 {
        run(&format!(
            r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {g}}} IN "creditos""#
        ));
    }
    run(
        r#"CREATE GHOST "g" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );
    let rollups_before = engine
        .turba
        .dictionary
        .prefix(&ROLLUP_PREFIX)
        .unwrap()
        .len();
    assert!(
        rollups_before > 0,
        "a spilled ghost must have rollup entries on disk"
    );

    run(r#"DROP GHOST "g""#);
    let rollups_after = engine
        .turba
        .dictionary
        .prefix(&ROLLUP_PREFIX)
        .unwrap()
        .len();
    assert_eq!(rollups_after, 0, "DROP must purge the rollup namespace");
}

/// Under BULKMODE, notify_write must not touch aggregate state at
/// all — neither the in-RAM map nor the on-disk rollups. The
/// per-record rollup RMW collapsed the scale-1 bulk load to ~tens of
/// records/s; the bulk contract defers aggregates to REFRESH.
/// Covering entry inserts must continue.
#[test]
fn bulkmode_skips_all_aggregate_maintenance() {
    // SAFETY: same value as all other setters of this knob.
    unsafe { std::env::set_var("XYZ_GHOST_SUMMARIES_MAX_GROUPS", "4") };
    let dir = tempfile::tempdir().unwrap();
    let engine = crate::engine::Engine::open(dir.path()).unwrap();
    let run = |s: &str| {
        engine
            .run(s)
            .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
    };
    run(r#"LOBE "creditos""#);
    run(
        r#"CREATE GHOST "g" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );
    run("BULKMODE ON");
    for g in 0..10 {
        run(&format!(
            r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {g}}} IN "creditos""#
        ));
    }
    let rollups = engine
        .turba
        .dictionary
        .prefix(&ROLLUP_PREFIX)
        .unwrap()
        .len();
    assert_eq!(rollups, 0, "bulk writes must not RMW rollups");
    let mut in_ram: usize = 0;
    engine.ghost_manager.for_each_ghost(|_, m| {
        if let Some(Residency::InRam(map)) = m.aggregate.as_ref().map(|a| &a.residency) {
            in_ram += map.len();
        }
    });
    assert_eq!(in_ram, 0, "bulk writes must not grow in-RAM summaries");
    let entries = engine
        .ghost_manager
        .with_ghost("g", |m| m.index_count)
        .unwrap();
    assert_eq!(
        entries, 10,
        "covering entry inserts must continue during bulk"
    );
    run("BULKMODE OFF");
}

/// Probe (Q4 regression): after the bench lifecycle (CREATE with
/// EMBED → bulk PUTs → REFRESH) every ghost entry must still carry
/// the embedded projection — value strictly longer than the spatial
/// key. Bare spatial-key-sized values mean the read path falls back
/// to one point-read per entry (the 0.9 ms → 115 ms Q4 profile).
#[test]
fn refresh_after_bulk_preserves_embedded_projection() {
    // SAFETY: same value as all other setters of this knob.
    unsafe { std::env::set_var("XYZ_GHOST_SUMMARIES_MAX_GROUPS", "4") };
    let dir = tempfile::tempdir().unwrap();
    let engine = crate::engine::Engine::open(dir.path()).unwrap();
    let run = |s: &str| {
        engine
            .run(s)
            .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
    };
    run(r#"LOBE "creditos""#);
    run(
        r#"CREATE GHOST "exp" FROM "creditos" WHERE _type = "Credit" AND status = "active" ORDER BY empresa_id GROUP BY empresa_id, rfc AGGREGATE sum(monto), count() EMBED rfc, empresa_id"#,
    );
    run("BULKMODE ON");
    for i in 0..30 {
        run(&format!(
            r#"PUT {{_type: "Credit", rfc: "R{i:03}", empresa_id: "E{i:03}", monto: {i}, status: "active"}} IN "creditos""#
        ));
    }
    run("BULKMODE OFF");
    run(r#"REFRESH GHOST "exp""#);

    let (projection_nonempty, prefix) = engine
        .ghost_manager
        .with_ghost("exp", |meta| {
            (!meta.projection.is_empty(), meta.ghost_id.to_be_bytes())
        })
        .expect("ghost exists");
    assert!(
        projection_nonempty,
        "meta.projection must survive REFRESH, got empty"
    );
    let ks = engine.ghost_manager.ks().unwrap();
    let mut checked = 0;
    for entry in ks.prefix_iter(&prefix).unwrap() {
        assert!(
            entry.value.len() > xyzdb_core::key::SPATIAL_KEY_SIZE,
            "entry value is bare spatial-key-sized ({}B) — projection lost",
            entry.value.len()
        );
        checked += 1;
    }
    assert_eq!(checked, 30, "all rebuilt entries inspected");
}

#[test]
fn test_embed_roundtrip() {
    let spatial_key = vec![0xAA; 18];
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("monto".into(), Value::Float(12345.67));
    fields.insert("rfc".into(), Value::Text("ABCD123456ABC".into()));
    fields.insert("status".into(), Value::Text("active".into()));
    fields.insert("extra".into(), Value::Int(999)); // not in projection

    let record = xyzdb_core::record::Record {
        lid: xyzdb_core::lid::LID::from_raw(0),
        lobe_name: "creditos".into(),
        fields,
        created_at: 0,
        updated_at: 0,
    };

    let projection = vec!["monto".into(), "rfc".into(), "status".into()];

    // Encode
    let encoded = encode_ghost_value(&spatial_key, &record, &projection);
    println!(
        "encoded len: {} (spatial=18, extra={})",
        encoded.len(),
        encoded.len() - 18
    );
    assert!(
        encoded.len() > 18,
        "EMBED should produce more than 18 bytes"
    );

    // Verify spatial key preserved
    assert_eq!(&encoded[..18], &spatial_key[..]);

    // Decode
    let decoded = decode_ghost_projection(&encoded, 18, &projection, "creditos");
    assert!(decoded.is_some(), "decode should succeed");

    let rec = decoded.unwrap();
    assert_eq!(rec.fields.get("monto"), Some(&Value::Float(12345.67)));
    assert_eq!(
        rec.fields.get("rfc"),
        Some(&Value::Text("ABCD123456ABC".into()))
    );
    assert_eq!(
        rec.fields.get("status"),
        Some(&Value::Text("active".into()))
    );
    assert!(
        !rec.fields.contains_key("extra"),
        "extra field should not be in projection"
    );

    println!("EMBED roundtrip OK: {} fields decoded", rec.fields.len());
}

#[test]
fn test_embed_empty_projection() {
    let spatial_key = vec![0xBB; 18];
    let record = xyzdb_core::record::Record {
        lid: xyzdb_core::lid::LID::from_raw(0),
        lobe_name: "test".into(),
        fields: std::collections::BTreeMap::new(),
        created_at: 0,
        updated_at: 0,
    };

    let encoded = encode_ghost_value(&spatial_key, &record, &[]);
    assert_eq!(
        encoded.len(),
        18,
        "empty projection should be spatial key only"
    );
}

/// The escape hatch that v0.2 dev iteration depends on: when
/// `PersistedGhostMeta` changes shape between commits (new field
/// added, field reordered, …), the format byte at the head of every
/// persisted record is bumped. `decode_persisted_ghost_meta` must
/// return `UnknownFormat` for every byte that isn't the current
/// `GHOST_META_FORMAT`, so `load_all` can skip cleanly.
///
/// Regression guard: if someone later decides the format byte is
/// "unnecessary" and rips it out, this test fires. Every schema
/// change from now on depends on it.
/// Skeleton GhostMeta with only the fields `bump_access` cares about set to
/// non-default values. Reused by both `bump_access` tests.
fn skeleton_meta(name: &str) -> GhostMeta {
    GhostMeta {
        name: name.into(),
        ghost_id: 1,
        version: 2,
        lobe_id: 1,
        source_lobe: "data".into(),
        filter: ast::FilterExpr::And(vec![]),
        order_by_field: String::new(),
        sort_inverted: false,
        metric_order: None,
        order_emitted_at: None,
        state: 1,
        index_count: 0,
        aggregate: None,
        projection: vec![],
        created_at: 0,
        last_accessed: 0,
        incremental_updates: 0,
        lifecycle: GhostLifecycle::Auto {
            class: AutoClass::Ephemeral,
            ttl_seconds: 86_400,
            telemetry: AccessTelemetry::default(),
        },
        core_filters_cache: None,
        maintenance_degraded: false,
    }
}

/// Regression guard for the `u64::MAX as i64 = -1` bug that would
/// have made every Permanent ghost trip the expiration check on the
/// first reaper tick. If anyone rewrites `identify_expired_ghosts`
/// to "simplify" the None bail-early into a sentinel-based check,
/// this test fires.
fn ephemeral_with(name: &str, source_lobe: &str, last_accessed: i64) -> GhostMeta {
    let mut m = skeleton_meta(name);
    m.source_lobe = source_lobe.into();
    m.last_accessed = last_accessed;
    m.set_lifecycle(GhostType::Ephemeral, Some(86_400));
    m
}

#[test]
fn identify_lru_returns_oldest_of_matching_type() {
    let mgr = GhostLobeManager::new();
    mgr.insert_ghost(ephemeral_with("g_a", "data", 3000));
    mgr.insert_ghost(ephemeral_with("g_b", "data", 1000));
    mgr.insert_ghost(ephemeral_with("g_c", "data", 2000));

    assert_eq!(
        mgr.identify_lru("data", GhostType::Ephemeral).as_deref(),
        Some("g_b")
    );
}

#[test]
fn identify_lru_respects_source_lobe() {
    let mgr = GhostLobeManager::new();
    mgr.insert_ghost(ephemeral_with("g_data", "data", 1000));
    mgr.insert_ghost(ephemeral_with("g_other", "other", 500));

    // g_other has older last_accessed but wrong lobe — LRU for "data" is g_data.
    assert_eq!(
        mgr.identify_lru("data", GhostType::Ephemeral).as_deref(),
        Some("g_data")
    );
}

#[test]
fn identify_lru_respects_ghost_type() {
    let mgr = GhostLobeManager::new();
    let mut perm = ephemeral_with("g_perm", "data", 100);
    perm.set_lifecycle(GhostType::Permanent, None);
    mgr.insert_ghost(perm);
    mgr.insert_ghost(ephemeral_with("g_eph", "data", 500));

    // g_perm has older last_accessed but type mismatch — only ephemeral counts.
    assert_eq!(
        mgr.identify_lru("data", GhostType::Ephemeral).as_deref(),
        Some("g_eph")
    );
    // And no ephemeral in "other".
    assert_eq!(mgr.identify_lru("other", GhostType::Ephemeral), None);
}

#[test]
fn identify_lru_empty_returns_none() {
    let mgr = GhostLobeManager::new();
    assert_eq!(mgr.identify_lru("data", GhostType::Ephemeral), None);
}

/// Tie-break is deterministic via `BTreeMap` iteration order (name asc).
/// Documented behavior — this test locks it in so a future refactor that
/// changes the backing map notices.
#[test]
fn identify_lru_ties_break_by_name_ascending() {
    let mgr = GhostLobeManager::new();
    mgr.insert_ghost(ephemeral_with("g_z", "data", 1000));
    mgr.insert_ghost(ephemeral_with("g_a", "data", 1000));
    mgr.insert_ghost(ephemeral_with("g_m", "data", 1000));

    // All three share last_accessed — min_by_key takes the first one it
    // encounters, which for BTreeMap is "g_a".
    assert_eq!(
        mgr.identify_lru("data", GhostType::Ephemeral).as_deref(),
        Some("g_a")
    );
}

#[test]
fn count_by_type_counts_correctly() {
    let mgr = GhostLobeManager::new();
    mgr.insert_ghost(ephemeral_with("g1", "data", 100));
    mgr.insert_ghost(ephemeral_with("g2", "data", 200));
    mgr.insert_ghost(ephemeral_with("g3", "other", 300));

    let mut perm = ephemeral_with("g4", "data", 400);
    perm.set_lifecycle(GhostType::Permanent, None);
    mgr.insert_ghost(perm);

    assert_eq!(mgr.count_by_type("data", GhostType::Ephemeral), 2);
    assert_eq!(mgr.count_by_type("data", GhostType::Permanent), 1);
    assert_eq!(mgr.count_by_type("other", GhostType::Ephemeral), 1);
    assert_eq!(mgr.count_by_type("missing", GhostType::Ephemeral), 0);
}

#[test]
fn identify_promotable_detects_seven_bits_set() {
    let mgr = GhostLobeManager::new();
    let mut meta = ephemeral_with("g_7d", "data", 100);
    meta.telemetry_mut().unwrap().daily_access_bitmap = 0b0111_1111; // bits 0-6 set
    mgr.insert_ghost(meta);

    assert_eq!(mgr.identify_promotable(), vec!["g_7d".to_string()]);
}

#[test]
fn identify_promotable_ignores_six_consecutive_bits() {
    let mgr = GhostLobeManager::new();
    let mut meta = ephemeral_with("g_6d", "data", 100);
    meta.telemetry_mut().unwrap().daily_access_bitmap = 0b0011_1111; // bits 0-5 only
    mgr.insert_ghost(meta);

    assert!(mgr.identify_promotable().is_empty());
}

#[test]
fn identify_promotable_ignores_high_bits_beyond_seven() {
    let mgr = GhostLobeManager::new();
    let mut meta = ephemeral_with("g_full", "data", 100);
    meta.telemetry_mut().unwrap().daily_access_bitmap = 0xFFFF_FFFF; // every bit set
    mgr.insert_ghost(meta);

    // All low 7 bits set → promotable. High bits don't block promotion.
    assert_eq!(mgr.identify_promotable(), vec!["g_full".to_string()]);
}

#[test]
fn identify_promotable_ignores_permanent_and_promoted() {
    let mgr = GhostLobeManager::new();
    let mut perm = ephemeral_with("g_perm", "data", 100);
    // Declared has no telemetry to carry a bitmap; it is inherently unpromotable.
    perm.set_lifecycle(GhostType::Permanent, None);
    mgr.insert_ghost(perm);

    let mut prom = ephemeral_with("g_prom", "data", 100);
    prom.set_lifecycle(GhostType::Promoted, None);
    prom.telemetry_mut().unwrap().daily_access_bitmap = 0x7F;
    mgr.insert_ghost(prom);

    assert!(mgr.identify_promotable().is_empty());
}

#[test]
fn permanent_ghost_never_expires_even_with_ancient_last_accessed() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g_permanent");
    meta.set_lifecycle(GhostType::Permanent, None);
    meta.last_accessed = 0; // Unix epoch — "ancient"
    mgr.insert_ghost(meta);

    let expired = mgr.identify_expired_ghosts();
    assert!(
        expired.is_empty(),
        "Permanent ghost must never be flagged expired"
    );
}

#[test]
fn fresh_ephemeral_is_not_expired() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g_fresh");
    meta.set_lifecycle(GhostType::Ephemeral, Some(86_400));
    meta.last_accessed = now_micros(); // just now
    mgr.insert_ghost(meta);

    assert!(mgr.identify_expired_ghosts().is_empty());
}

#[test]
fn stale_ephemeral_is_expired() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g_stale");
    meta.set_lifecycle(GhostType::Ephemeral, Some(86_400)); // 24h TTL
    meta.last_accessed = 0; // 1970 — way past
    meta.lobe_id = 7;
    mgr.insert_ghost(meta);

    let expired = mgr.identify_expired_ghosts();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].name, "g_stale");
    assert_eq!(expired[0].lobe_id, 7);
}

#[test]
fn ephemeral_within_ttl_is_not_expired() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g_recent");
    meta.set_lifecycle(GhostType::Ephemeral, Some(86_400));
    // 12h ago — half the TTL
    meta.last_accessed = now_micros() - (12 * 3600 * 1_000_000);
    mgr.insert_ghost(meta);

    assert!(mgr.identify_expired_ghosts().is_empty());
}

/// NTP can jump the clock backward; virtualized time is even less
/// reliable. A `last_accessed` value that appears to be in the
/// future (greater than `now`) must not trip the expiration check —
/// saturating_sub + max(0) collapses negative-elapsed to zero.
#[test]
fn clock_skew_backward_does_not_expire() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g_skew");
    meta.set_lifecycle(GhostType::Ephemeral, Some(60));
    meta.last_accessed = now_micros() + (3600 * 1_000_000); // 1h in future
    mgr.insert_ghost(meta);

    assert!(mgr.identify_expired_ghosts().is_empty());
}

#[test]
fn rotate_bitmaps_advances_day_shifts_bits() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g");
    meta.telemetry_mut().unwrap().daily_access_bitmap = 0b0111_1111; // accessed last 7 days
    mgr.insert_ghost(meta);

    let mut last = 10_i64;
    mgr.rotate_bitmaps_if_needed(11, &mut last);

    assert_eq!(last, 11, "last_rotation advances");
    let bitmap = mgr
        .with_ghost("g", |m| m.telemetry().unwrap().daily_access_bitmap)
        .unwrap();
    assert_eq!(bitmap, 0b0011_1111, "bit 0 drops, bit 6 becomes bit 7");
}

#[test]
fn rotate_bitmaps_same_day_is_noop() {
    let mgr = GhostLobeManager::new();
    let mut meta = skeleton_meta("g");
    meta.telemetry_mut().unwrap().daily_access_bitmap = 0b1010_1010;
    mgr.insert_ghost(meta);

    let mut last = 42_i64;
    mgr.rotate_bitmaps_if_needed(42, &mut last);

    assert_eq!(last, 42, "last_rotation unchanged");
    let bitmap = mgr
        .with_ghost("g", |m| m.telemetry().unwrap().daily_access_bitmap)
        .unwrap();
    assert_eq!(bitmap, 0b1010_1010, "bitmap unchanged on same-day tick");
}

#[test]
fn rotate_bitmaps_on_empty_manager_does_not_panic() {
    let mgr = GhostLobeManager::new();
    let mut last = 0_i64;
    mgr.rotate_bitmaps_if_needed(1, &mut last);
    assert_eq!(last, 1);
}

#[test]
fn bump_access_updates_tracking_fields() {
    let mgr = GhostLobeManager::new();
    mgr.insert_ghost(skeleton_meta("g"));

    mgr.bump_access("g");
    mgr.bump_access("g");
    mgr.bump_access("g");

    mgr.with_ghost("g", |meta| {
        let telemetry = meta.telemetry().expect("auto ghost has telemetry");
        assert_eq!(telemetry.access_count_total, 3);
        assert_eq!(telemetry.daily_access_bitmap & 1, 1, "today bit set");
        assert!(
            meta.last_accessed > 0,
            "last_accessed bumped to a real timestamp"
        );
    })
    .expect("ghost still present");
}

#[test]
fn bump_access_missing_ghost_is_noop() {
    // The ghost could have been evicted by the reaper between the scan
    // picking it and the telemetry writeback — must not panic.
    let mgr = GhostLobeManager::new();
    mgr.bump_access("does-not-exist");
    assert!(mgr.is_empty());
}

#[test]
fn unknown_format_byte_is_not_corrupt() {
    // Magic + unknown format byte + arbitrary trailing bytes.
    let mut val = Vec::new();
    val.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
    val.push(0xFF); // definitely not the current format
    val.extend_from_slice(&[0x00, 0x01, 0x02]); // whatever

    match decode_persisted_ghost_meta(&val) {
        DecodedMeta::UnknownFormat { found } => assert_eq!(found, 0xFF),
        DecodedMeta::Ok(_) => panic!("should not decode a record with unknown format"),
        DecodedMeta::Corrupt(_) => panic!("unknown format must be distinct from corruption"),
    }
}

#[test]
fn missing_magic_is_unknown_format() {
    let val = [0xAA, 0xBB, 0xCC];
    assert!(matches!(
        decode_persisted_ghost_meta(&val),
        DecodedMeta::UnknownFormat { .. }
    ));
}

#[test]
fn current_format_with_valid_payload_round_trips() {
    // Hand-construct a valid PersistedGhostMeta, serialize it with the
    // production byte layout, and make sure the decoder returns Ok.
    let persisted = PersistedGhostMeta {
        name: "g_alive".into(),
        ghost_id: 7,
        version: 2,
        lobe_id: 1,
        source_lobe: "data".into(),
        is_auto: false,
        filter: PersistedFilterExpr::And(vec![]),
        order_by_field: String::new(),
        sort_inverted: false,
        metric_order: None,
        order_emitted_at: None,
        state: 1,
        index_count: 0,
        aggregate_specs: vec![],
        global_aggregates: Default::default(),
        group_fields: vec![],
        group_summaries: Default::default(),
        spilled: false,
        projection: vec![],
        created_at: 0,
        last_accessed: 0,
        incremental_updates: 0,
        ghost_type: GhostType::Permanent,
        ttl_seconds: None,
        daily_access_bitmap: 0,
        access_count_total: 0,
    };

    let payload = postcard::to_allocvec(&persisted).expect("serialize");
    let mut val = Vec::with_capacity(3 + payload.len());
    val.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
    val.push(GHOST_META_FORMAT);
    val.extend_from_slice(&payload);

    match decode_persisted_ghost_meta(&val) {
        DecodedMeta::Ok(p) => {
            assert_eq!(p.name, "g_alive");
            assert_eq!(p.ghost_type, GhostType::Permanent);
            assert_eq!(p.ttl_seconds, None);
        }
        other => panic!(
            "expected Ok, got {:?}",
            match other {
                DecodedMeta::Ok(_) => "Ok",
                DecodedMeta::UnknownFormat { .. } => "UnknownFormat",
                DecodedMeta::Corrupt(_) => "Corrupt",
            }
        ),
    }
}

/// P2-2 teeth: ghost membership reads the memoised core-filter tree and never
/// reconverts the AST per call. Proven by mutating the AST `filters` AFTER the
/// cache is built and confirming a second `ensure_core_filters` is a no-op —
/// the cache still reflects the ORIGINAL filter, so the write path cannot be
/// deep-cloning literals per write. This is the fix whose regression prompted
/// the evaluator unification; the suite guards it explicitly from here on.
#[test]
fn core_filter_cache_is_authoritative_and_built_once() {
    use xytalk_parser::ast::{Filter, FilterOp as AstOp, Literal};

    fn rec_status(s: &str) -> xyzdb_core::record::Record {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("status".to_string(), Value::Text(s.into()));
        xyzdb_core::record::Record {
            lid: xyzdb_core::lid::LID::from_raw(0),
            lobe_name: "l".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        }
    }

    let mut meta = skeleton_meta("g");
    meta.filter = ast::FilterExpr::Condition(Filter {
        field: "status".into(),
        op: AstOp::Eq,
        value: Literal::Text("active".into()),
    });
    meta.ensure_core_filters();

    let active = rec_status("active");
    let inactive = rec_status("inactive");
    let cache = meta.core_filters_cache.as_ref().unwrap();
    assert!(crate::ops::matches_core_expr(&active, cache));
    assert!(!crate::ops::matches_core_expr(&inactive, cache));

    // Mutate the AST filters to the OPPOSITE predicate, then ensure again. The
    // tree is immutable per ghost → the guard makes this a no-op; if the write
    // path ever rebuilt from `filters`, `active` would stop matching.
    meta.filter = ast::FilterExpr::Condition(Filter {
        field: "status".into(),
        op: AstOp::Eq,
        value: Literal::Text("inactive".into()),
    });
    meta.ensure_core_filters();
    let cache = meta.core_filters_cache.as_ref().unwrap();
    assert!(
        crate::ops::matches_core_expr(&active, cache),
        "membership must read the memoised cache, never reconvert AST per write (P2-2)"
    );
    assert!(!crate::ops::matches_core_expr(&inactive, cache));
}

/// TANDA B decoupling, BY CONSTRUCTION (not by timing): ghosts in different
/// lobes live behind DIFFERENT shard locks, so holding one lobe's write lock
/// cannot block a write to another lobe's ghosts. A write to lobe A never
/// acquires lobe B's lock — proven by distinct `Arc` identities and by a
/// `try_write` on B succeeding while A's lock is held.
#[test]
fn writes_to_different_lobes_use_distinct_shard_locks() {
    let mgr = GhostLobeManager::new();
    let mut a = skeleton_meta("g_lobe_a");
    a.lobe_id = 1;
    let mut b = skeleton_meta("g_lobe_b");
    b.lobe_id = 2;
    mgr.insert_ghost(a);
    mgr.insert_ghost(b);

    let shard_a = mgr.lobe_shard(1).expect("lobe 1 shard exists");
    let shard_b = mgr.lobe_shard(2).expect("lobe 2 shard exists");
    assert!(
        !std::sync::Arc::ptr_eq(&shard_a, &shard_b),
        "each lobe must have its own shard lock (guaranteed, not probabilistic, decoupling)"
    );

    // Hold lobe 1's write lock; lobe 2's shard must still be writable.
    let _held_a = shard_a.write();
    assert!(
        shard_b.try_write().is_some(),
        "a write holding lobe 1's shard lock must not block lobe 2's shard"
    );
}

/// TANDA B read-your-write: a mutation to a ghost is visible on the very next
/// read through the shard (synchronous, not deferred) — the sharded path keeps
/// the same consistency the single global lock had.
#[test]
fn shard_path_is_read_your_write() {
    let mgr = GhostLobeManager::new();
    mgr.insert_ghost(skeleton_meta("g"));
    // A mutation lands...
    mgr.with_ghost_mut("g", |m| m.index_count = 42);
    // ...and is visible immediately on the next read (same thread, no barrier).
    let seen = mgr.with_ghost("g", |m| m.index_count);
    assert_eq!(seen, Some(42), "a write must be visible on the next read");
}
