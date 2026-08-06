//! 1g regression — a record-returning SCAN must never be routed to a
//! row-collapsing ghost (GROUP BY / AGGREGATE).
//!
//! Repro of the v0.7.2 content-gate finding: with an aggregating ghost
//! present (`… GROUP BY g AGGREGATE count()`), a plain record scan whose
//! filter matched the ghost's predicate was routed to it. The ghost holds
//! one summary row per group, so the scan returned a single row per group
//! instead of every underlying record — silently dropping data that was
//! stored and otherwise retrievable. The fix: the router serves record
//! scans only from non-aggregating (covering-index) ghosts.
//!
//! Domain-neutral vocab throughout: the engine is agnostic. `_type` is the
//! engine's own record discriminator, not a domain field.

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
fn record_scan_not_routed_to_aggregating_ghost() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#);

    // Two records share one gravity key (`*group`) and one secondary value
    // (`_type = "thing"`), differing only by identity. Both must stay
    // retrievable by `WHERE group = X AND _type = "thing"`.
    exec(
        &engine,
        r#"PUT {*group: "G1", _type: "thing", id: "A", n: 1} IN "items""#,
    );
    exec(
        &engine,
        r#"PUT {*group: "G1", _type: "thing", id: "B", n: 2} IN "items""#,
    );
    // Some sibling rows so the gravity bucket is not trivial.
    for i in 0..8 {
        exec(
            &engine,
            &format!(r#"PUT {{*group: "G1", _type: "other", id: "O{i}", n: {i}}} IN "items""#),
        );
    }

    // A ROW-COLLAPSING ghost over the same predicate: one summary row per
    // group. This is what the router must NOT serve a record scan from.
    exec(
        &engine,
        r#"CREATE GHOST "things_by_group" FROM "items" WHERE _type = "thing" ORDER BY group GROUP BY group AGGREGATE count()"#,
    );

    // The aggregate query the ghost legitimately serves still works (sanity:
    // the ghost exists and is routable for its own shape).
    let _ = exec(
        &engine,
        r#"SCAN "items" WHERE _type = "thing" GROUP BY group | AGGREGATE count()"#,
    );

    // The bug: this record scan's `_type = "thing"` predicate matches the
    // ghost, so pre-fix it routed to `things_by_group` and returned 1 row
    // (the group) instead of 2 records. Post-fix it must read the records.
    let n = count(exec(
        &engine,
        r#"SCAN "items" WHERE group = "G1" AND _type = "thing""#,
    ));
    assert_eq!(
        n, 2,
        "a record scan must return both matching records, never be collapsed \
         by an aggregating ghost (got {n})"
    );
}
