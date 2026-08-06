//! D1 regression guard — gravity-as-index must find value-only-placed records.
//!
//! Pre-0.8 the engine placed records under two divergent gravity-hash
//! conventions:
//!   * `*field` gravity  → `compute_gravity_hash` = hash(name \0 value)
//!   * anchor / LINK      → value-only = hash(value)
//! while a `WHERE field = X` scan computed the name+value hash. A record placed
//! value-only (anchor/LINK path) landed in a different bucket than the one the
//! scan probed — the gravity-bounded scan silently missed it, even though it
//! existed and was reachable by anchor.
//!
//! D1 unifies every path on the canonical **value-only** hash (one primitive,
//! `compute_gravity_hash`), so placement and the scan fast path cannot diverge.
//! This test, RED before D1, now guards that they don't.
//!
//! Domain-neutral vocab: the engine is agnostic.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    let stmt = xytalk_parser::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e:?}"));
    engine
        .execute(stmt)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn count(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("unexpected scan result: {other:?}"),
    }
}

#[test]
fn gravity_scan_finds_anchor_placed_record() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#);
    exec(&engine, r#"ANCHOR "key" UNIQUE IN "items""#);

    // P1: `*key` gravity → placed at the name+value bucket. This also
    // registers `key` as the lobe's gravity field, so later `WHERE key = X`
    // scans take the gravity-indexed fast path.
    exec(&engine, r#"PUT {*key: "P1", tag: "gravity"} IN "items""#);
    // P2: no `*`, anchored on `key` → placed at the value-only bucket.
    exec(&engine, r#"PUT {key: "P2", tag: "anchor"} IN "items""#);

    // Both records exist and are reachable by anchor (dictionary lookup):
    assert_eq!(
        count(exec(&engine, r#"FIND "items" WHERE key = "P2""#)),
        1,
        "anchor lookup finds the value-only-placed record (sanity)"
    );

    // The gravity-indexed scan probes the name+value bucket; P2 lives in the
    // value-only bucket → it is missed. Post-D1 this must return 1.
    let n = count(exec(&engine, r#"SCAN "items" WHERE key = "P2""#));
    assert_eq!(
        n, 1,
        "gravity scan must find the anchor-placed record (got {n}); the \
         value-only vs name+value hash asymmetry (D1) makes it miss"
    );
}
