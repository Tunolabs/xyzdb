//! #11 — keel-omit health signal (case C), driven through the REAL engine path
//! (verb PUT / SCAN over the product form: GRAVITY BY declared). Additive
//! observability: this asserts the counters + omit_ratio on the stats surface
//! and that placement/recall semantics are unchanged (the omitting record still
//! lands and is recoverable by an unfiltered SCAN, and is correctly excluded
//! from WHERE <field> = X). The placement contract itself
//! (`raw_missing_field_returns_none` in gravity_spec.rs) is not touched.

use xyzdb_engine::engine::{Engine, QueryResult};

fn count(engine: &Engine, query: &str) -> usize {
    match engine.run(query).expect("query") {
        QueryResult::Records(v) => v.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// (keel_present, keel_absent, omit_ratio) for a lobe from the stats surface.
fn keel(engine: &Engine, lobe: &str) -> Option<(u64, u64, f64)> {
    engine
        .stats_snapshot()
        .keel_health
        .into_iter()
        .find(|e| e.lobe == lobe)
        .map(|e| (e.keel_present, e.keel_absent, e.omit_ratio))
}

#[test]
fn keel_present_then_absent_updates_counters_and_ratio() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    engine.run(r#"LOBE "mem""#).expect("lobe");
    engine
        .run(r#"GRAVITY BY bucket IN "mem""#)
        .expect("gravity");

    // PUT carrying the declared keel (plain field, no `*`) → keel_present, ratio 0.
    engine
        .run(r#"PUT {bucket: "b1", note: "x"} IN "mem""#)
        .expect("put present");
    assert_eq!(
        keel(&engine, "mem"),
        Some((1, 0, 0.0)),
        "keel present, ratio stable at 0"
    );

    // PUT omitting the keel (case C) → keel_absent, ratio rises.
    engine
        .run(r#"PUT {note: "y"} IN "mem""#)
        .expect("put absent");
    let (present, absent, ratio) = keel(&engine, "mem").expect("entry");
    assert_eq!((present, absent), (1, 1), "one present, one absent");
    assert!(
        (ratio - 0.5).abs() < 1e-9,
        "omit_ratio rose to 0.5, got {ratio}"
    );

    // Semantics unchanged: BOTH records recoverable by an unfiltered SCAN.
    assert_eq!(
        count(&engine, r#"SCAN "mem" LIMIT 1000"#),
        2,
        "omitting record still recoverable"
    );
    // The co-located one is found by the scoped query; the omitting one is
    // correctly excluded (it has no `bucket` field) — the silent under-recall
    // the counter now makes visible.
    assert_eq!(
        count(&engine, r#"SCAN "mem" WHERE bucket = "b1" LIMIT 1000"#),
        1
    );
}

#[test]
fn anchor_present_but_gravity_field_absent_counts_absent() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    engine.run(r#"LOBE "m2""#).expect("lobe");
    engine.run(r#"ANCHOR "id" UNIQUE IN "m2""#).expect("anchor");
    engine.run(r#"GRAVITY BY bucket IN "m2""#).expect("gravity");

    // Anchor present, gravity field absent: co-locates by the anchor axis, NOT
    // the declared keel → misplacement, counted as keel_absent.
    engine
        .run(r#"PUT {id: "k1", note: "x"} IN "m2""#)
        .expect("put");
    assert_eq!(keel(&engine, "m2"), Some((0, 1, 1.0)));
    assert_eq!(
        count(&engine, r#"SCAN "m2" LIMIT 1000"#),
        1,
        "record still present"
    );
}

#[test]
fn put_batch_plain_gravity_field_co_locates_like_single_put() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    engine.run(r#"LOBE "mem""#).expect("lobe");
    engine
        .run(r#"GRAVITY BY bucket IN "mem""#)
        .expect("gravity");

    // Plain gravity field (no `*`), written via BATCH. The single-PUT path honors
    // a declared GRAVITY BY for a plain field (efbe49e); the batch path must too,
    // or scoped queries silently under-recall. Regression guard for the batch/bulk
    // placement fix (was: else branch resolved via compute_record_gravity_hash,
    // skipping the declared spec → WHERE found nothing).
    engine
        .run(r#"PUT BATCH IN "mem" [{bucket: "b1", id: "g1"}, {bucket: "b1", id: "g2"}]"#)
        .expect("batch");

    // The scoped query must find both records (they carry bucket=b1) via the keel
    // fast path — i.e. batch placed them in the same bucket single PUT would.
    assert_eq!(
        count(&engine, r#"SCAN "mem" WHERE bucket = "b1" LIMIT 1000"#),
        2,
        "plain gravity field via BATCH must co-locate like single PUT"
    );
}

#[test]
fn put_batch_counts_keel_per_record_not_per_batch() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    engine.run(r#"LOBE "mem""#).expect("lobe");
    engine
        .run(r#"GRAVITY BY bucket IN "mem""#)
        .expect("gravity");

    // One batch: 6 records carry the keel (*bucket), 4 omit it (case C). Batch is
    // the high-volume write path, so this MUST count per record — per-batch would
    // record absent=1 and dilute the ratio on the path that matters most.
    let mut recs: Vec<String> = Vec::new();
    for i in 0..6 {
        recs.push(format!(r#"{{*bucket: "b1", id: "p{i}"}}"#));
    }
    for i in 0..4 {
        recs.push(format!(r#"{{id: "a{i}"}}"#));
    }
    let batch = format!(r#"PUT BATCH IN "mem" [{}]"#, recs.join(", "));
    engine.run(&batch).expect("batch");

    let (present, absent, ratio) = keel(&engine, "mem").expect("entry");
    assert_eq!(
        (present, absent),
        (6, 4),
        "per-record: 6 present, 4 absent (NOT 1)"
    );
    assert!((ratio - 0.4).abs() < 1e-9, "omit_ratio = 0.4, got {ratio}");
    assert_eq!(
        count(&engine, r#"SCAN "mem" LIMIT 1000"#),
        10,
        "all recoverable"
    );
}

#[test]
fn non_gravity_lobe_is_excluded_from_the_denominator() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    engine.run(r#"LOBE "plain""#).expect("lobe");
    // No GRAVITY BY → not a keel candidate.
    engine.run(r#"PUT {a: "1"} IN "plain""#).expect("put");
    engine.run(r#"PUT {a: "2"} IN "plain""#).expect("put");
    assert_eq!(
        keel(&engine, "plain"),
        None,
        "non-gravity lobe must not appear in keel_health"
    );
}
