//! 2b regression — concurrent same-anchor PUTs must not duplicate/orphan.
//!
//! There is a TOCTOU window between the anchor uniqueness check
//! (`dictionary.get` in `ops::put`) and the batch commit that writes the
//! anchor. Pre-fix, N threads racing the SAME anchor all pass the check before
//! any of them commits → N records land (one anchor-reachable, the rest
//! orphaned in the spatial keyspace: SCAN sees N, FIND-by-anchor sees 1).
//! Post-fix (sharded check→commit lock keyed by dict_key), exactly one
//! survives and there are no orphans.

// SPDX-License-Identifier: BUSL-1.1
use std::sync::{Arc, Barrier};
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> Result<QueryResult, String> {
    let stmt = xytalk_parser::parse(s).map_err(|e| format!("parse {s:?}: {e:?}"))?;
    engine
        .execute(stmt)
        .map_err(|e| format!("exec {s:?}: {e:?}"))
}

fn scan_count(engine: &Engine, lobe: &str) -> usize {
    match exec(engine, &format!(r#"SCAN "{lobe}""#)).expect("scan") {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("unexpected scan result: {other:?}"),
    }
}

#[test]
fn concurrent_same_anchor_put_keeps_exactly_one() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    // Domain-neutral names: the engine is agnostic to any application domain.
    exec(&engine, r#"LOBE "items""#).expect("declare lobe");
    exec(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).expect("declare anchor");
    let engine = engine.into_arc();

    const N: usize = 8;
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let e = Arc::clone(&engine);
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Pre-parse so the barrier releases all threads straight into
                // the engine's check→commit window, not parser jitter.
                let stmt = format!(r#"PUT {{id: "DUP", data: "t{i}"}} IN "items""#);
                b.wait();
                exec(&e, &stmt).is_ok()
            })
        })
        .collect();

    let successes = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|&ok| ok)
        .count();

    // The declared anchor-uniqueness guarantee: exactly one PUT survives, and
    // no orphan is left behind in the spatial keyspace.
    assert_eq!(
        successes, 1,
        "exactly one same-anchor PUT must succeed (got {successes})"
    );
    assert_eq!(
        scan_count(&engine, "items"),
        1,
        "no orphan records: SCAN must see exactly one"
    );
}

/// 2b-bulk (deterministic, no concurrency): a single PUT BATCH that contains
/// two records with the SAME anchor must be rejected — neither in-flight record
/// is committed yet, so the per-record `dictionary.get` check cannot see the
/// other. Pre-fix, both are written silently (uniqueness violated).
#[test]
fn put_batch_with_duplicate_anchor_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#).expect("declare lobe");
    exec(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).expect("declare anchor");

    let res = exec(
        &engine,
        r#"PUT BATCH IN "items" [{id: "DUP", data: "a"}, {id: "DUP", data: "b"}]"#,
    );
    assert!(
        res.is_err(),
        "a batch with a duplicate anchor must be rejected, got {res:?}"
    );
    assert_eq!(
        scan_count(&engine, "items"),
        0,
        "a rejected batch must write nothing (got {} records)",
        scan_count(&engine, "items")
    );
}

/// 2b-bulk (b) — concurrent PUT BATCHes racing the SAME anchor (one record
/// each, so intra-batch dedup does not apply) must serialize on the anchor
/// shard: exactly one batch survives, no orphans. Pre-fix the per-batch
/// `dictionary.get` races and several commit.
#[test]
fn concurrent_put_batch_same_anchor_keeps_exactly_one() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#).expect("declare lobe");
    exec(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).expect("declare anchor");
    let engine = engine.into_arc();

    const N: usize = 8;
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let e = Arc::clone(&engine);
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let stmt = format!(r#"PUT BATCH IN "items" [{{id: "DUP", data: "t{i}"}}]"#);
                b.wait();
                exec(&e, &stmt).is_ok()
            })
        })
        .collect();

    let successes = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|&ok| ok)
        .count();

    assert_eq!(
        successes, 1,
        "exactly one concurrent same-anchor batch must succeed (got {successes})"
    );
    assert_eq!(
        scan_count(&engine, "items"),
        1,
        "no orphan records across batches"
    );
}

/// SET on a declared anchor field must be rejected. SET does not maintain the
/// anchor dictionary index, so editing identity in place would leave a stale,
/// no-longer-unique index. The record must be left untouched.
#[test]
fn set_on_anchor_field_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#).expect("declare lobe");
    exec(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).expect("declare anchor");
    exec(&engine, r#"PUT {id: "A", data: "v"} IN "items""#).expect("put");

    let res = exec(&engine, r#"SET "items" id = "B" WHERE id = "A""#);
    assert!(
        res.is_err(),
        "SET on an anchor field must be rejected, got {res:?}"
    );

    // The record is intact and still resolves by its ORIGINAL anchor; the
    // rejected new value was never indexed.
    assert_eq!(
        scan_count(&engine, "items"),
        1,
        "the record must be left intact"
    );
    match exec(&engine, r#"FIND "items" WHERE id = "A""#).expect("find") {
        QueryResult::Records(r) => {
            assert_eq!(r.len(), 1, "original anchor must still resolve")
        }
        other => panic!("unexpected find result: {other:?}"),
    }

    // A non-anchor SET on the same record still works (the guard is precise).
    exec(&engine, r#"SET "items" data = "w" WHERE id = "A""#)
        .expect("SET on a non-anchor field must still succeed");
}
