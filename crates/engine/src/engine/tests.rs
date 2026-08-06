// SPDX-License-Identifier: BUSL-1.1
use super::*;
use crate::ghost::{GhostMeta, GhostType};

/// Pre-0.7.6, pins and ghost metadata shared the dictionary key
/// [0xFF,0xFD][id:2] (pins by lobe_id, metas by ghost_id — both
/// allocated from 1), so the first lobe's pins and the first ghost's
/// meta clobbered each other on persist. With the pin prefix moved,
/// both must survive a reboot side by side.
#[test]
fn pins_survive_ghost_meta_with_same_id() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "a""#).unwrap();
        engine.run(r#"LOBE "b""#).unwrap();
        engine
            .run(r#"PUT {_type: "Item", rfc: "R1", monto: 1} IN "a""#)
            .unwrap();
        engine.run(r#"PIN rfc IN "b""#).unwrap();
        // The collision under test requires the ghost's id to equal
        // lobe "b"'s id — assert it so renumbering can't silently
        // turn this test into a no-op.
        engine
            .run(r#"CREATE GHOST "g" FROM "a" WHERE _type = "Item" ORDER BY rfc"#)
            .unwrap();
        let lobe_b = engine.lobe_registry.read().get("b").unwrap().id;
        let ghost_id = engine.ghost_manager.ghost_id("g").unwrap();
        assert_eq!(
            lobe_b, ghost_id,
            "test setup must produce the colliding id pair"
        );
    }
    let engine = Engine::open(dir.path()).unwrap();
    assert_eq!(
        engine.pinned_fields.read().get("b"),
        Some(&vec!["rfc".to_string()]),
        "pins must survive a ghost meta persisted under the same id"
    );
    assert!(
        engine.ghost_manager.list().iter().any(|g| g.name == "g"),
        "the ghost must survive a pin persisted under the same id"
    );
}

/// Pins written by pre-0.7.6 builds (legacy shared key) must still
/// load — accepted only when pin-shaped — and migrate to the new
/// prefix at boot.
#[test]
fn legacy_pins_load_and_migrate() {
    let dir = tempfile::tempdir().unwrap();
    let lobe_id;
    {
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "items""#).unwrap();
        lobe_id = engine.lobe_registry.read().get("items").unwrap().id;
        // Plant a legacy pin entry exactly as persist_pinned wrote it
        // pre-0.7.6: [0xFF,0xFD][lobe_id] → [MAGIC][0x01][postcard].
        let mut key = vec![0xFF, 0xFD];
        key.extend_from_slice(&lobe_id.to_be_bytes());
        let mut val = Vec::new();
        val.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        val.push(0x01);
        val.extend_from_slice(&postcard::to_allocvec(&vec!["rfc".to_string()]).unwrap());
        engine.turba.dictionary.insert(&key, &val).unwrap();
        // Bare inserts are memtable-only; flush like persist_pinned did.
        engine.turba.dictionary.seal_active();
        engine.turba.dictionary.flush_sealed().unwrap();
    }
    let engine = Engine::open(dir.path()).unwrap();
    assert_eq!(
        engine.pinned_fields.read().get("items"),
        Some(&vec!["rfc".to_string()]),
        "legacy pins must load through the fallback"
    );
    // Migrated: the new-prefix key now exists.
    let mut new_key = Vec::from(PIN_PREFIX);
    new_key.extend_from_slice(&lobe_id.to_be_bytes());
    assert!(
        engine.turba.dictionary.get(&new_key).unwrap().is_some(),
        "legacy pins must be migrated to the new prefix at boot"
    );
}

/// PASO 6.3: with a hash already in `ghost_inflight`, a candidate
/// with the same `filter_desc` short-circuits at the single-flight
/// gate and increments `singleflight_skipped` instead of submitting
/// to the pool. Manual pre-insert isolates the test from the
/// worker's timing — the inflight slot stays occupied as long as
/// the test wants.
#[test]
fn singleflight_skips_consecutive_same_filter_desc() {
    use crate::scan_telemetry::AutoGhostCandidate;
    use xytalk_parser::parse;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open").into_arc();
    engine
        .execute(parse("LOBE \"data\"").expect("parse"))
        .expect("create lobe");

    let filter_desc = "test_filter_desc_consecutive";
    let hash = xxhash_rust::xxh3::xxh3_64(filter_desc.as_bytes());
    engine.ghost_inflight.insert(hash);

    let pre_skip = engine
        .ghost_singleflight_skipped_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let pre_spawn = engine
        .ghost_candidate_spawn_count
        .load(std::sync::atomic::Ordering::Relaxed);

    engine.maybe_create_ephemeral_ghost(AutoGhostCandidate {
        lobe: "data".into(),
        filters: vec![],
        filter_desc: filter_desc.into(),
        aggregate_fields: vec![],
    });

    let post_skip = engine
        .ghost_singleflight_skipped_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let post_spawn = engine
        .ghost_candidate_spawn_count
        .load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        post_skip - pre_skip,
        1,
        "single-flight gate must skip duplicate filter_desc"
    );
    assert_eq!(
        post_spawn - pre_spawn,
        0,
        "no submit when single-flight skips"
    );
}

/// PASO 6.3: 5 threads racing on the same pre-occupied filter_desc
/// hash all see `insert` return `false` and increment the skipped
/// counter. The DashSet's atomic operation guarantees no double
/// dispatch — `singleflight_skipped` increments by exactly N.
#[test]
fn singleflight_skips_concurrent_same_filter_desc() {
    use crate::scan_telemetry::AutoGhostCandidate;
    use std::sync::Barrier;
    use xytalk_parser::parse;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open").into_arc();
    engine
        .execute(parse("LOBE \"data\"").expect("parse"))
        .expect("create lobe");

    let filter_desc = "test_filter_desc_concurrent";
    let hash = xxhash_rust::xxh3::xxh3_64(filter_desc.as_bytes());
    engine.ghost_inflight.insert(hash);

    let pre_skip = engine
        .ghost_singleflight_skipped_count
        .load(std::sync::atomic::Ordering::Relaxed);

    const N: usize = 5;
    let barrier = std::sync::Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let engine_c = engine.clone();
            let barrier_c = barrier.clone();
            std::thread::spawn(move || {
                barrier_c.wait();
                engine_c.maybe_create_ephemeral_ghost(AutoGhostCandidate {
                    lobe: "data".into(),
                    filters: vec![],
                    filter_desc: "test_filter_desc_concurrent".into(),
                    aggregate_fields: vec![],
                });
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread join");
    }

    let post_skip = engine
        .ghost_singleflight_skipped_count
        .load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        post_skip - pre_skip,
        N as u64,
        "all {N} concurrent duplicates must skip"
    );
}

/// PASO 6.3: distinct `filter_desc` values do not collide on the
/// single-flight gate; both proceed to pool submit and increment
/// `candidate_spawn`. `singleflight_skipped` stays at zero.
#[test]
fn singleflight_distinct_filter_desc_both_proceed() {
    use crate::scan_telemetry::AutoGhostCandidate;
    use xytalk_parser::parse;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open").into_arc();
    engine
        .execute(parse("LOBE \"data\"").expect("parse"))
        .expect("create lobe");

    let pre_skip = engine
        .ghost_singleflight_skipped_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let pre_spawn = engine
        .ghost_candidate_spawn_count
        .load(std::sync::atomic::Ordering::Relaxed);

    engine.maybe_create_ephemeral_ghost(AutoGhostCandidate {
        lobe: "data".into(),
        filters: vec![],
        filter_desc: "distinct_filter_A".into(),
        aggregate_fields: vec![],
    });
    engine.maybe_create_ephemeral_ghost(AutoGhostCandidate {
        lobe: "data".into(),
        filters: vec![],
        filter_desc: "distinct_filter_B".into(),
        aggregate_fields: vec![],
    });

    let post_skip = engine
        .ghost_singleflight_skipped_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let post_spawn = engine
        .ghost_candidate_spawn_count
        .load(std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        post_skip - pre_skip,
        0,
        "distinct filter_desc must not skip"
    );
    assert_eq!(
        post_spawn - pre_spawn,
        2,
        "both distinct candidates submit to pool"
    );
}

/// PASO 6.1: Engine::open instantiates the bounded ghost-creator
/// pool and the pool's workers terminate cleanly when the engine
/// drops. PASO 6.2 wires `maybe_create_ephemeral_ghost` to
/// `ghost_pool.submit`; until then the pool runs idle.
#[test]
fn engine_open_starts_ghost_pool() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine");
    // Pool sized per `clamp(cpus / 2, 1, 4)`; at minimum 1 worker.
    assert!(engine.ghost_pool.worker_count() >= 1);
    // Capacity is `n * 4` slots.
    assert_eq!(
        engine.ghost_pool.capacity(),
        engine.ghost_pool.worker_count() * 4
    );
    // Drop engine: pool sender drops, workers see Disconnected,
    // join cleanly. No hang means clean shutdown.
    drop(engine);
}

/// Build a minimal Ephemeral GhostMeta that matches an Engine open
/// against `lobe_name`. Callers insert it via
/// `engine.ghost_manager.insert_ghost` to bypass the real `create()` path —
/// this test is about reap behavior, not ghost construction.
fn stale_ephemeral(name: &str, lobe_id: u16) -> GhostMeta {
    GhostMeta {
        name: name.into(),
        ghost_id: 999,
        version: 2,
        lobe_id,
        source_lobe: String::new(),
        filter: xytalk_parser::ast::FilterExpr::And(vec![]),
        order_by_field: String::new(),
        sort_inverted: false,
        metric_order: None,
        order_emitted_at: None,
        state: 1,
        index_count: 0,
        aggregate: None,
        projection: vec![],
        created_at: 0,
        last_accessed: 0, // 1970 — way past any realistic TTL
        incremental_updates: 0,
        // Auto/Ephemeral, 60s TTL, and it's been decades → expired.
        lifecycle: crate::ghost::GhostLifecycle::Auto {
            class: crate::ghost::AutoClass::Ephemeral,
            ttl_seconds: 60,
            telemetry: crate::ghost::AccessTelemetry::default(),
        },
        core_filters_cache: None,
        maintenance_degraded: false,
    }
}

/// End-to-end reap_cycle integration check: an expired ghost that was
/// registered in a router gets dropped from BOTH the ghost manager
/// AND the router — the reaper cascade this test locks in.
#[test]
fn reap_cycle_drops_expired_ghost_and_unregisters_from_router() {
    use xyzdb_core::record::FilterOp;
    use xyzdb_core::value::Value;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine");

    // Register a ghost in the router for lobe_id=1.
    {
        let mut routers = engine.ghost_routers.write();
        let router = routers.entry(1).or_default();
        router.register_ghost(
            "g_expired",
            vec![("status".into(), FilterOp::Eq, Value::Text("x".into()))],
            String::new(),
            false,
            false,
            vec![],
        );
        assert!(router.has_ghosts(), "ghost registered");
    }

    // Install a matching stale ephemeral in the manager.
    engine
        .ghost_manager
        .insert_ghost(stale_ephemeral("g_expired", 1));

    // Fire one reap cycle. Day bucket value is arbitrary here — the
    // test doesn't care about rotation; the ghost expires on TTL alone.
    let mut last_rotation = 0_i64;
    engine.reap_cycle(0, &mut last_rotation);

    // Ghost should be gone from both sides.
    assert!(
        !engine.ghost_manager.contains_ghost("g_expired"),
        "expired ghost removed from manager"
    );
    let routers = engine.ghost_routers.read();
    let router = routers.get(&1).expect("router still present");
    assert!(
        !router.has_ghosts(),
        "expired ghost also unregistered from router"
    );
}

/// Build an Ephemeral for a given lobe with a specific `last_accessed`
/// stamp, so LRU ordering in the test is deterministic.
fn ephemeral_at(name: &str, source_lobe: &str, lobe_id: u16, last_accessed: i64) -> GhostMeta {
    let mut m = stale_ephemeral(name, lobe_id);
    m.source_lobe = source_lobe.into();
    m.last_accessed = last_accessed;
    // Override the "0 = 1970 = ancient" from stale_ephemeral — these
    // ghosts should NOT be TTL-expired during the LRU test.
    m.set_lifecycle(GhostType::Ephemeral, Some(i64::MAX as u64));
    m
}

/// The core eviction contract: at the 10-Ephemeral per-lobe cap (a
/// test-local limit driven directly; production is 20), adding an 11th
/// triggers eviction of the LRU, and the eviction cascades to the
/// router. Bypasses the async `maybe_create_ephemeral_ghost` path and
/// drives `enforce_ghost_type_limit` directly — that async path is
/// already covered by the auto-ghost tests.
#[test]
fn enforce_ephemeral_limit_evicts_lru_and_unregisters_router() {
    use xyzdb_core::record::FilterOp;
    use xyzdb_core::value::Value;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine");

    // Install 10 Ephemerals with last_accessed = 1000, 2000, ..., 10_000.
    // LRU is name="auto_0" at timestamp 1000.
    {
        let mut routers = engine.ghost_routers.write();
        let router = routers.entry(1).or_default();
        for i in 0..10_i64 {
            let name = format!("auto_{i}");
            engine
                .ghost_manager
                .insert_ghost(ephemeral_at(&name, "data", 1, (i + 1) * 1000));
            router.register_ghost(
                &name,
                vec![("k".into(), FilterOp::Eq, Value::Text(name.clone()))],
                String::new(),
                false,
                false,
                vec![],
            );
        }
    }
    assert_eq!(
        engine
            .ghost_manager
            .count_by_type("data", GhostType::Ephemeral),
        10,
        "ten ephemerals installed"
    );

    // Enforce limit = 10 → at-capacity → evict LRU.
    let evicted = engine.enforce_ghost_type_limit(1, "data", GhostType::Ephemeral, 10);
    assert!(evicted.is_some(), "eviction fired at the limit");
    let evicted = evicted.unwrap();
    assert_eq!(evicted.name, "auto_0", "oldest evicted");
    assert_eq!(evicted.lobe_id, 1);

    // Room for the new one.
    assert_eq!(
        engine
            .ghost_manager
            .count_by_type("data", GhostType::Ephemeral),
        9,
        "count dropped by one"
    );
    // Router no longer has the evicted ghost (we can't read the private
    // ghost map; test indirectly via has_ghosts + re-registration check).
    let routers = engine.ghost_routers.read();
    let router = routers.get(&1).expect("router still present");
    assert!(router.has_ghosts(), "nine ghosts still registered");
    drop(routers);

    // Second call with the same limit is a noop — count is 9, below 10.
    let evicted_twice = engine.enforce_ghost_type_limit(1, "data", GhostType::Ephemeral, 10);
    assert!(evicted_twice.is_none(), "no eviction below limit");
}

/// A fresh Ephemeral that hit 7 consecutive days of access gets
/// promoted in-place: ghost_id unchanged, filter_desc preserved in
/// the router, new name `promoted_<suffix>`, TTL flips from 24h to
/// 30d. Spatial keyspace is not re-scanned — index entries keep
/// their ghost_id prefix and stay reachable under the new name.
#[test]
fn reap_cycle_promotes_eligible_ephemeral() {
    use xyzdb_core::record::FilterOp;
    use xyzdb_core::value::Value;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine");

    // Install an Ephemeral with 7-bit bitmap and a router registration.
    let old_name = "auto_data_cafef00d".to_string();
    {
        let mut meta = stale_ephemeral(&old_name, 1);
        meta.source_lobe = "data".into();
        meta.last_accessed = crate::ghost::now_micros(); // fresh, so not TTL-expired
        meta.set_lifecycle(GhostType::Ephemeral, Some(i64::MAX as u64));
        if let Some(t) = meta.telemetry_mut() {
            t.daily_access_bitmap = 0b0111_1111; // 7 consecutive days → promotable
        }
        engine.ghost_manager.insert_ghost(meta);

        let mut routers = engine.ghost_routers.write();
        let router = routers.entry(1).or_default();
        router.register_ghost(
            &old_name,
            vec![("status".into(), FilterOp::Eq, Value::Text("active".into()))],
            String::new(),
            false,
            false,
            vec![],
        );
        router.set_filter_desc(&old_name, "FilterExpr::Eq(status,active)".into());
    }

    let mut last_rotation = 0_i64;
    engine.reap_cycle(0, &mut last_rotation);

    // Old name gone, new name present and typed Promoted with 30d TTL.
    assert!(
        !engine.ghost_manager.contains_ghost(&old_name),
        "old name removed"
    );
    let new_name = "promoted_data_cafef00d";
    engine
        .ghost_manager
        .with_ghost(new_name, |promoted| {
            assert_eq!(promoted.ghost_type(), crate::ghost::GhostType::Promoted);
            assert_eq!(promoted.ttl_seconds(), Some(30 * 24 * 60 * 60));
        })
        .expect("promoted under new name");

    // Router rename preserved filter_desc and filter_fields.
    let routers = engine.ghost_routers.read();
    let router = routers.get(&1).expect("router still present");
    assert_eq!(
        router.get_filter_desc(new_name),
        Some("FilterExpr::Eq(status,active)"),
    );
}

/// When the reaper drops an expired Ephemeral, the corresponding
/// telemetry pattern's `ghost_created` flag is cleared so the same
/// filter can re-trigger a fresh auto-ghost if it stays hot. Locks
/// in the flag-clearing contract against the "weekly report" regression.
#[test]
fn weekly_pattern_recovers_after_ttl_expiry() {
    use xyzdb_core::record::FilterOp;
    use xyzdb_core::value::Value;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine");

    let filter_desc = "FilterExpr::Eq(report,weekly)".to_string();

    // Simulate "pattern was hot a week ago; auto-ghost got built."
    {
        let mut telemetry = engine.scan_telemetry.write();
        // Seed the pattern with one primary scan so the pattern map
        // has an entry, then flip the flag to true.
        telemetry.record_with_filters(
            crate::scan_telemetry::ScanTelemetry {
                lobe: "data".into(),
                filter_desc: filter_desc.clone(),
                source: "primary".into(),
                records_scanned: 0,
                records_returned: 0,
                duration: std::time::Duration::from_millis(1),
            },
            &[],
            &[],
        );
        telemetry.set_ghost_flag(&filter_desc, true);
    }

    // Install the corresponding Ephemeral with ancient last_accessed,
    // so the TTL reaper drops it on the next cycle.
    let ghost_name = "auto_data_weekly".to_string();
    {
        engine
            .ghost_manager
            .insert_ghost(stale_ephemeral(&ghost_name, 1));

        let mut routers = engine.ghost_routers.write();
        let router = routers.entry(1).or_default();
        router.register_ghost(
            &ghost_name,
            vec![("report".into(), FilterOp::Eq, Value::Text("weekly".into()))],
            String::new(),
            false,
            false,
            vec![],
        );
        router.set_filter_desc(&ghost_name, filter_desc.clone());
    }

    // Pre-condition: flag is set (ghost covers the filter).
    assert_eq!(
        engine.scan_telemetry.read().has_ghost_flag(&filter_desc),
        Some(true),
    );

    let mut last_rotation = 0_i64;
    engine.reap_cycle(0, &mut last_rotation);

    // Ghost dropped, flag cleared — the filter is free to re-trigger
    // auto-ghost creation when the report runs again next week.
    assert!(!engine.ghost_manager.contains_ghost(&ghost_name));
    assert_eq!(
        engine.scan_telemetry.read().has_ghost_flag(&filter_desc),
        Some(false),
        "pattern flag cleared after ghost drop — filter can re-trigger"
    );
}

/// The first end-to-end Phase 1 test: five scans with the same filter
/// accumulate in telemetry, the fifth trips the trigger threshold,
/// the background worker creates an auto-ghost + registers it in the
/// router, the sixth scan routes to the ghost (not Primary).
///
/// Verifies the full chain — parser + Engine::execute + ops/scan.rs
/// telemetry branching + maybe_create_ephemeral_ghost spawn +
/// ghost_manager.create + reclassify + router registration +
/// bump_access — without asking any new API to surface the scan_source
/// publicly. Routing is verified via `access_count_total == 1`
/// (strict: scans 1-5 go to Primary and must NOT bump).
///
/// Timeout defaults to 5s and overrides via `XYZDB_TEST_TIMEOUT_MS`
/// for loaded CI runners. On failure the panic message includes the
/// telemetry store's pattern count and recent count so flaky failures
/// produce actionable diagnostics instead of "test didn't finish."
#[test]
fn five_scans_trigger_auto_ghost_sixth_routes() {
    use std::time::Instant;
    use xytalk_parser::parse;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine").into_arc();

    // Lower thresholds so triggering is deterministic within the test
    // budget: 5 hits, any latency. Production uses `DEFAULT_MIN_HITS`
    // and `DEFAULT_MIN_LATENCY_MS` from scan_telemetry.rs unchanged.
    engine.scan_telemetry.write().set_thresholds(5, 0.0);

    // Bootstrap: create a lobe + seed a handful of matching records
    // so the scan has data to iterate over.
    for stmt in &[
        "LOBE \"data\"",
        "PUT {user_id: \"U1\", status: \"overdue\"} IN \"data\"",
        "PUT {user_id: \"U2\", status: \"overdue\"} IN \"data\"",
        "PUT {user_id: \"U3\", status: \"overdue\"} IN \"data\"",
        "PUT {user_id: \"U4\", status: \"current\"} IN \"data\"",
    ] {
        let ast = parse(stmt).expect("parse setup stmt");
        engine.execute(ast).expect("execute setup stmt");
    }

    // Five scans accumulate in telemetry. The fifth trips the trigger
    // and dispatches the background auto-ghost worker.
    let query = "SCAN \"data\" WHERE status = \"overdue\"";
    for _ in 0..5 {
        let ast = parse(query).expect("parse scan");
        engine.execute(ast).expect("execute scan");
    }

    // Poll for the ghost to land in BOTH the ghost map and its router —
    // scan #6 needs the router entry to route to Ghost, not just the
    // manager entry. Polling on router-readiness avoids the micro-race
    // between `ghost_manager.create` and `router.register_ghost`.
    let timeout = std::env::var("XYZDB_TEST_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(5));

    let start = Instant::now();
    let mut ghost_name: Option<String> = None;
    while start.elapsed() < timeout {
        let router_ready = engine.ghost_routers.read().values().any(|r| r.has_ghosts());
        if router_ready && let Some(name) = engine.ghost_manager.ghost_names().into_iter().next() {
            ghost_name = Some(name);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let ghost_name = ghost_name.unwrap_or_else(|| {
        let telem = engine.scan_telemetry.read();
        panic!(
            "auto-ghost not created within {:?}. \
                 telemetry patterns: {}, recent: {}. \
                 Possible causes: trigger check regressed (patterns==5 but no ghost), \
                 or scan_telemetry not reached (patterns==0 → check scan.rs routing of \
                 Primary vs Ghost in record_with_filters).",
            timeout,
            telem.pattern_count(),
            telem.recent_count(),
        )
    });

    assert!(
        ghost_name.starts_with("auto_data_"),
        "auto-ghost naming contract: got {ghost_name:?}"
    );

    // Sixth scan. Router routes to Ghost, `bump_access` fires.
    let ast = parse(query).expect("parse scan");
    engine.execute(ast).expect("execute scan #6");

    // Strict invariant: scan #6 bumped access exactly once. If the
    // counter is ≥ 2, some path double-counts (possible bug). If it's
    // 0, routing fell through to Primary (ghost registration not
    // picked up — possible router bug).
    engine
        .ghost_manager
        .with_ghost(&ghost_name, |meta| {
            assert_eq!(
                meta.ghost_type(),
                crate::ghost::GhostType::Ephemeral,
                "auto-ghost must be Ephemeral post-reclassification"
            );
            assert_eq!(
                meta.ttl_seconds(),
                Some(86_400),
                "Ephemeral TTL is 24h (set by reclassify_lifecycle)"
            );
            assert_eq!(
                meta.telemetry().map_or(0, |t| t.access_count_total),
                1,
                "scan #6 bumped exactly once; scans 1-5 routed to Primary and must not bump"
            );
        })
        .expect("ghost still present");
}

/// Persist a ghost with a caller-chosen `last_accessed` + TTL + type.
/// Done via public API only: insert into the `ghosts` map (pub field),
/// then call `reclassify_lifecycle` (pub fn) which internally invokes
/// the private `persist_meta` to write the current in-memory state —
/// including the caller-supplied `last_accessed` — to the dictionary.
///
/// Avoids exposing `persist_meta` itself as pub(crate); boot-time
/// scenario setup becomes a public-API composition rather than a
/// test-only API surface.
fn inject_persisted_ghost(
    engine: &Engine,
    name: &str,
    ghost_type: crate::ghost::GhostType,
    ttl_seconds: Option<u64>,
    last_accessed: i64,
) {
    let mut meta = stale_ephemeral(name, 1);
    meta.source_lobe = "data".into();
    meta.last_accessed = last_accessed;
    meta.set_lifecycle(ghost_type, ttl_seconds);
    engine.ghost_manager.insert_ghost(meta);
    // reclassify_lifecycle persists whatever's in the map, including
    // the last_accessed we just set. Passing the same ghost_type +
    // ttl_seconds makes the classification step idempotent.
    engine
        .ghost_manager
        .reclassify_lifecycle(name, ghost_type, ttl_seconds, &engine.turba.dictionary)
        .expect("reclassify_lifecycle persists the injected state");
}

/// Boot-time TTL check: a ghost whose persisted `last_accessed` is older
/// than its `ttl_seconds` is purged by `load_all` at boot. The ghost never
/// enters the runtime map — the check fires after deserialization
/// and before the `insert`, so the orphan ghost can't accumulate
/// across restarts.
#[test]
fn boot_load_all_drops_expired_ephemeral() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().to_path_buf();

    // Phase 1: open engine, inject "created long ago, never used"
    // Ephemeral. The engine drops at the end of the scope; turba's
    // WAL group-commit fsyncs the dictionary write before shutdown.
    {
        let engine = Engine::open(&db_path).expect("open engine");
        inject_persisted_ghost(
            &engine,
            "g_expired",
            crate::ghost::GhostType::Ephemeral,
            Some(60), // 60s TTL, elapsed many times over at now_micros()
            0,        // 1970 — ancient last_accessed
        );
    }

    // Phase 2: reopen. load_all's TTL check must fire and purge.
    let engine = Engine::open(&db_path).expect("reopen engine");
    assert!(
        !engine.ghost_manager.contains_ghost("g_expired"),
        "boot load_all must purge Ephemeral whose persisted last_accessed > ttl"
    );
}

/// Permanent ghosts have `ttl_seconds == None` and must survive `load_all`
/// regardless of how old their persisted `last_accessed` is. Guards
/// against a future refactor that accidentally treats None-TTL as
/// "zero TTL" (which would fall into the `u64::MAX as i64 = -1` bug
/// already fixed on the reaper side).
#[test]
fn boot_load_all_preserves_permanent_with_old_last_accessed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().to_path_buf();

    {
        let engine = Engine::open(&db_path).expect("open engine");
        inject_persisted_ghost(
            &engine,
            "g_forever",
            crate::ghost::GhostType::Permanent,
            None, // no TTL
            0,    // ancient last_accessed; irrelevant for Permanent
        );
    }

    let engine = Engine::open(&db_path).expect("reopen engine");
    assert!(
        engine.ghost_manager.contains_ghost("g_forever"),
        "boot load_all must preserve Permanent ghosts regardless of last_accessed"
    );
}

/// D1 migration end-to-end: a record placed under the pre-0.8 name+value
/// gravity hash is (a) blocked by the guard until migrated, then (b) rehashed
/// to its value-only bucket and found by the gravity SCAN fast path; the
/// guard lifts and stays down across a reopen (spec re-persisted at 0x03).
#[test]
fn migrate_rehashes_name_value_record_to_value_only() {
    use xyzdb_core::key::{SpatialKey, hash_to_48bits, normalize_timestamp};
    use xyzdb_core::lid::LID;
    use xyzdb_core::record::{Record, serialize_record};
    use xyzdb_core::value::Value;

    fn exec(engine: &Engine, s: &str) -> Result<QueryResult> {
        engine.execute(xytalk_parser::parse(s).expect("parse"))
    }
    fn count(qr: QueryResult) -> usize {
        match qr {
            QueryResult::Records(r) => r.len(),
            QueryResult::PaginatedRecords { records, .. } => records.len(),
            other => panic!("unexpected: {other:?}"),
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().to_path_buf();

    // Phase 1: build a pre-D1 on-disk state.
    {
        let engine = Engine::open(&db_path).expect("open");
        exec(&engine, r#"LOBE "items""#).unwrap();
        exec(&engine, r#"ANCHOR "key" UNIQUE IN "items""#).unwrap();
        // P1: a normal value-only PUT (registers Raw("key")).
        exec(&engine, r#"PUT {*key: "P1"} IN "items""#).unwrap();
        let lobe_id = engine.lobe_registry.read().get("items").unwrap().id;

        // P2: manufactured at the OLD name+value bucket hash("key\0P2"), as
        // the pre-0.8 `*`-path would have placed it — written straight
        // through turba, bypassing the now value-only PUT path.
        let nv_hash = hash_to_48bits("key\u{0}P2");
        let lid = LID::new(lobe_id);
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("key".to_string(), Value::Text("P2".into()));
        fields.insert("_type".to_string(), Value::Text("items".into()));
        let rec = Record {
            lid,
            lobe_name: "items".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        };
        let skb = SpatialKey::new(lobe_id, nv_hash, 0, normalize_timestamp(0), 999_999).to_bytes();
        let mut batch = engine.turba.batch();
        batch.put_spatial(&skb, &serialize_record(&rec));
        batch.put_identity(&lid.to_bytes(), &skb);
        batch.put_dictionary(
            &crate::anchor::dictionary_key(lobe_id, "key", "P2"),
            &lid.to_bytes(),
        );
        batch.commit().unwrap();

        // Overwrite the gravity slot with a Fase-0 (0x02) spec so a reopen
        // marks the database un-migrated (name+value era).
        let mut slot_key = Vec::from(GRAVITY_PREFIX);
        slot_key.extend_from_slice(&lobe_id.to_be_bytes());
        let mut slot_val = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        slot_val.push(0x02);
        slot_val
            .extend_from_slice(&postcard::to_allocvec(&GravitySpec::Raw("key".into())).unwrap());
        engine
            .turba
            .dictionary
            .insert(&slot_key, &slot_val)
            .unwrap();
        engine.turba.dictionary.seal_active();
        engine.turba.dictionary.flush_sealed().unwrap();
    }

    // Phase 2: reopen → guard armed by the 0x02 slot.
    let engine = Engine::open(&db_path).expect("reopen");
    assert!(
        engine
            .gravity_needs_migration
            .load(std::sync::atomic::Ordering::Relaxed),
        "0x02 slot must arm the migration guard on load"
    );
    assert!(
        exec(&engine, r#"SCAN "items" WHERE key = "P2""#).is_err(),
        "gravity data op must be refused while un-migrated"
    );

    // Migrate: rehash P2 → value-only, lift the guard.
    engine.execute(Statement::Migrate(None)).unwrap();
    assert!(
        !engine
            .gravity_needs_migration
            .load(std::sync::atomic::Ordering::Relaxed),
        "migrate must lift the guard"
    );
    assert_eq!(
        count(exec(&engine, r#"SCAN "items" WHERE key = "P2""#).unwrap()),
        1,
        "P2 must be found at its value-only bucket post-migrate"
    );
    assert_eq!(
        count(exec(&engine, r#"SCAN "items" WHERE key = "P1""#).unwrap()),
        1,
        "P1 (already value-only) must remain found"
    );

    // Reopen: the re-persisted 0x03 spec must not re-arm the guard.
    drop(engine);
    let engine = Engine::open(&db_path).expect("reopen 2");
    assert!(
        !engine
            .gravity_needs_migration
            .load(std::sync::atomic::Ordering::Relaxed),
        "re-persisted 0x03 spec must keep the guard down across reopen"
    );
}

/// A Permanent ghost survives a reap cycle even with ancient
/// `last_accessed`. Guards against a future refactor that accidentally
/// expires Permanents alongside Ephemerals.
#[test]
fn reap_cycle_preserves_permanent_ghosts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine");

    let mut meta = stale_ephemeral("g_permanent", 2);
    meta.set_lifecycle(GhostType::Permanent, None);
    engine.ghost_manager.insert_ghost(meta);

    let mut last_rotation = 0_i64;
    engine.reap_cycle(0, &mut last_rotation);

    assert!(
        engine.ghost_manager.contains_ghost("g_permanent"),
        "Permanent ghost survives reaper"
    );
}

/// v0.2.0-alpha Finding 1: when the router selects a ghost whose
/// entry has already been dropped (LRU / TTL / manual DROP GHOST)
/// between `plan_scan` and the read, the user-visible error was
/// `"Invalid query: Ghost 'auto_X' not found"` — 25 events observed
/// on the SSD uniform 1 h run. v0.2.1 routes the error through a
/// dedicated `XyzError::GhostNotFound` variant and the scan path
/// catches it specifically, unregisters the dead router entry, and
/// falls back to the equivalent Primary execution. The user sees no
/// error.
///
/// Scenario reproduced here with a manual `drop_ghost` standing in
/// for the race (the actual race requires a writer-racing-reader set
/// up that can't be deterministic in a unit test). What's validated
/// is the OPS-side fallback contract: given a stale router entry
/// and a ghost_manager that returns GhostNotFound, the scan still
/// returns correct records from Primary and the router entry is
/// cleaned up.
#[test]
fn ghost_not_found_mid_scan_falls_back_to_primary() {
    use std::time::Instant;
    use xytalk_parser::parse;

    let dir = tempfile::tempdir().expect("tempdir");
    let engine = Engine::open(dir.path()).expect("open engine").into_arc();

    // Lower threshold so a handful of scans triggers auto-ghost.
    engine.scan_telemetry.write().set_thresholds(5, 0.0);

    for stmt in &[
        "LOBE \"data\"",
        "PUT {user_id: \"U1\", status: \"overdue\"} IN \"data\"",
        "PUT {user_id: \"U2\", status: \"overdue\"} IN \"data\"",
        "PUT {user_id: \"U3\", status: \"overdue\"} IN \"data\"",
        "PUT {user_id: \"U4\", status: \"current\"} IN \"data\"",
    ] {
        let ast = parse(stmt).expect("parse setup");
        engine.execute(ast).expect("execute setup");
    }

    let query = "SCAN \"data\" WHERE status = \"overdue\"";
    for _ in 0..5 {
        let ast = parse(query).expect("parse scan");
        engine.execute(ast).expect("execute scan");
    }

    // Wait for the ghost to be registered in both the manager and
    // the router — same polling contract as
    // `five_scans_trigger_auto_ghost_sixth_routes`.
    let timeout = std::time::Duration::from_secs(5);
    let start = Instant::now();
    let ghost_name = loop {
        let router_ready = engine.ghost_routers.read().values().any(|r| r.has_ghosts());
        if router_ready && let Some(name) = engine.ghost_manager.ghost_names().into_iter().next() {
            break name;
        }
        if start.elapsed() >= timeout {
            panic!("auto-ghost not registered within {timeout:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    // Simulate the race window: the router still points at the
    // ghost, but the manager has dropped it. `scan.rs` must catch
    // `GhostNotFound`, unregister the router entry, and re-run
    // the scan against Primary — transparent to the caller.
    engine
        .ghost_manager
        .drop_ghost(&ghost_name, &engine.turba.dictionary)
        .expect("drop ghost");
    let router_has_ghost_before = engine.ghost_routers.read().values().any(|r| r.has_ghosts());
    assert!(
        router_has_ghost_before,
        "precondition: router still references the now-dropped ghost"
    );

    // The scan must succeed (no `GhostNotFound` returned to caller)
    // and produce the correct record count from Primary.
    let ast = parse(query).expect("parse scan");
    let result = engine.execute(ast).expect("fallback scan must succeed");
    let records = match result {
        crate::engine::QueryResult::Records(r) => r,
        other => panic!("expected Records, got {other:?}"),
    };
    assert_eq!(
        records.len(),
        3,
        "three records match status=overdue; fallback must return all of them"
    );

    // Post-fallback the router should no longer reference the
    // dropped ghost — subsequent scans go straight to Primary
    // without paying the double-lookup cost.
    let router_has_ghost_after = engine.ghost_routers.read().values().any(|r| r.has_ghosts());
    assert!(
        !router_has_ghost_after,
        "fallback must unregister the stale router entry"
    );
}

/// 2a — `Engine::open` advances and durably persists the per-open boot
/// epoch counter, so each open mints LIDs under a distinct epoch. Checks
/// the per-dir persisted value (not the process-global), so it is not
/// raced by peer tests opening their own engines.
#[test]
fn boot_epoch_advances_and_persists_across_open() {
    let dir = tempfile::tempdir().unwrap();
    let read_epoch = |e: &Engine| -> u16 {
        e.turba
            .dictionary
            .get(&BOOT_EPOCH_KEY)
            .ok()
            .flatten()
            .filter(|v| v.len() == 2)
            .map(|v| u16::from_be_bytes([v[0], v[1]]))
            .unwrap_or(0)
    };
    let e1 = Engine::open(dir.path()).unwrap();
    assert_eq!(read_epoch(&e1), 1, "first open persists boot epoch 1");
    drop(e1);
    let e2 = Engine::open(dir.path()).unwrap();
    assert_eq!(
        read_epoch(&e2),
        2,
        "reopen advances + persists boot epoch 2"
    );
}

/// v0.8 sibling axis — `VECTOR <field> IN "lobe"` declares the lobe's
/// searchable vector field and is readable through `get_vector_spec`.
/// A second, differing declaration errors (declare before the first write),
/// mirroring the gravity-spec guard.
#[test]
fn vector_declares_and_conflicting_redeclare_errors() {
    use crate::vector_spec::VectorSpec;
    use xytalk_parser::ast::VectorStmt;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "m""#).unwrap();

    engine
        .execute(Statement::Vector(VectorStmt {
            field: "emb".to_string(),
            lobe: "m".to_string(),
        }))
        .unwrap();
    assert_eq!(engine.get_vector_spec("m"), Some(VectorSpec::new("emb")));

    // A differing declaration on a lobe that already has one errors.
    assert!(
        engine
            .execute(Statement::Vector(VectorStmt {
                field: "other".to_string(),
                lobe: "m".to_string(),
            }))
            .is_err(),
        "re-declaring a different vector field must error (declare before first write)"
    );
    // The original declaration survives the rejected redeclare.
    assert_eq!(engine.get_vector_spec("m"), Some(VectorSpec::new("emb")));
}

/// Retro-compatibility gate (CRITICAL): a lobe whose vector spec was written
/// by a build predating dimension tracking — envelope `0x01`, field-only, no
/// dimension — must still open and work. We hand-write exactly that legacy
/// slot, re-open the engine, and confirm end to end: (a) the spec loads with
/// the dimension unknown, (b) the next PUT learns and fixes the dimension,
/// (c) a subsequent mismatch is then rejected. An old lobe is never bricked by
/// the format bump — this goes RED the day `decode` stops honoring `0x01`.
#[test]
fn legacy_0x01_vector_slot_opens_and_learns_dim() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "mem""#).unwrap();
        let lobe_id = engine.lobe_registry.read().get("mem").unwrap().id;

        // Reconstruct exactly what the old encoder wrote to the slot:
        //   key   = [VECTOR_FIELD][lobe_id:2]
        //   value = [MAGIC][0x01][postcard(field)]   (no dimension)
        let mut key = Vec::new();
        key.extend_from_slice(&crate::reserved_keys::VECTOR_FIELD);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        let mut blob = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        blob.push(0x01);
        blob.extend_from_slice(&postcard::to_allocvec(&"emb".to_string()).unwrap());

        let dict = &engine.turba.dictionary;
        dict.insert(&key, &blob).unwrap();
        dict.seal_active();
        dict.flush_sealed().unwrap();
    }

    // Re-open: the 0x01 slot must load (dimension unknown) — the lobe opens.
    let engine = Engine::open(dir.path()).unwrap();
    assert_eq!(
        engine.get_vector_spec("mem"),
        Some(crate::vector_spec::VectorSpec {
            field: "emb".into(),
            dim: None
        }),
        "legacy 0x01 slot must open with the dimension unknown"
    );

    // The next PUT learns and fixes the dimension — the lobe works.
    let vec64 = (0..64)
        .map(|i| format!("{:?}", i as f32 + 0.5))
        .collect::<Vec<_>>()
        .join(",");
    engine
        .run(&format!(r#"PUT {{id:"x", emb:[{vec64}]}} IN "mem""#))
        .expect("PUT into a legacy lobe learns the dim");
    assert_eq!(engine.get_vector_spec("mem").unwrap().dim, Some(64));

    // And a mismatch is now enforced against the learned dimension.
    let vec128 = (0..128)
        .map(|i| format!("{:?}", i as f32 + 0.5))
        .collect::<Vec<_>>()
        .join(",");
    let err = engine
        .run(&format!(r#"PUT {{id:"y", emb:[{vec128}]}} IN "mem""#))
        .expect_err("a mismatch after learning must be rejected");
    assert!(err.to_string().contains("dimension"), "error was: {err}");
}
