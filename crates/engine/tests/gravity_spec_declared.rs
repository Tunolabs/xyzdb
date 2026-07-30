//! v0.8 keel — `GRAVITY BY <expr> IN "lobe"`.
//!
//! Declares a lobe's gravity spec explicitly. Writes route through it (the keel,
//! wired in put.rs), so a `Normalized(lower)` spec co-locates case-variants in
//! the same gravity bucket. Reads stay exact — normalization changes physical
//! placement, not query equality. Re-declaring a different spec after one exists
//! errors (declare before the first write; re-bucketing is re-gravitation).

use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn count(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn gravity_by_normalized_lower_keeps_reads_exact() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "users""#);
    exec(&engine, r#"GRAVITY BY lower(empresa) IN "users""#);
    // Both case variants land (co-located by lower(empresa); not asserted here —
    // that's a physical property covered by the gravity_spec unit tests).
    exec(&engine, r#"PUT {*empresa: "Acme", id: 1} IN "users""#);
    exec(&engine, r#"PUT {*empresa: "acme", id: 2} IN "users""#);

    // Reads stay exact: the stored field value is unchanged by normalization.
    assert_eq!(
        count(exec(&engine, r#"SCAN "users" WHERE empresa = "Acme""#)),
        1,
        "exact filter returns only the matching-case row"
    );
    assert_eq!(
        count(exec(&engine, r#"SCAN "users""#)),
        2,
        "both rows are stored"
    );
}

#[test]
fn gravity_by_composite_declares_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "docs""#);
    exec(&engine, r#"GRAVITY BY (tenant, doc) IN "docs""#);
    exec(
        &engine,
        r#"PUT {*tenant: "t1", doc: "d1", body: "x"} IN "docs""#,
    );
    assert_eq!(count(exec(&engine, r#"SCAN "docs""#)), 1);
}

#[test]
fn gravity_by_composite_query_full_tuple_vs_partial() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "docs""#);
    exec(&engine, r#"GRAVITY BY (tenant, doc) IN "docs""#);
    // Two docs of the same tenant land in different (tenant,doc) buckets.
    exec(
        &engine,
        r#"PUT {*tenant: "t1", doc: "d1", body: "x"} IN "docs""#,
    );
    exec(
        &engine,
        r#"PUT {*tenant: "t1", doc: "d2", body: "y"} IN "docs""#,
    );

    // Full tuple → gravity fast path (3d-3 routes Composite through the keel),
    // bounded to the (t1, d1) bucket; returns exactly that record.
    assert_eq!(
        count(exec(
            &engine,
            r#"SCAN "docs" WHERE tenant = "t1" AND doc = "d1""#
        )),
        1,
        "full-tuple query pins the bucket and returns the matching doc"
    );
    // Partial tuple does not pin → full scan, still correct (both t1 docs).
    assert_eq!(
        count(exec(&engine, r#"SCAN "docs" WHERE tenant = "t1""#)),
        2,
        "partial tuple falls back to a correct full scan"
    );
}

#[test]
fn gravity_by_conflicting_redeclare_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "x""#);
    exec(&engine, r#"GRAVITY BY rfc IN "x""#);
    assert!(
        engine.run(r#"GRAVITY BY lower(rfc) IN "x""#).is_err(),
        "re-declaring a different gravity spec must error (declare before first write)"
    );
}

#[test]
fn multi_gravity_markers_without_composite_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "docs""#);
    // Two `*` markers but no declared composite spec → loud error, not a
    // silent collapse to the first field.
    assert!(
        engine
            .run(r#"PUT {*tenant: "t1", *doc: "d1", body: "x"} IN "docs""#)
            .is_err(),
        "multiple gravity markers without a composite spec must error"
    );
    // The failed PUT left no spurious Raw spec behind: declaring the composite
    // now still succeeds (it would error if a different spec were registered).
    exec(&engine, r#"GRAVITY BY (tenant, doc) IN "docs""#);
}

#[test]
fn multi_gravity_markers_with_composite_ok() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "docs""#);
    exec(&engine, r#"GRAVITY BY (tenant, doc) IN "docs""#);
    // With the composite declared, the same multi-`*` PUT is accepted and the
    // record is found via the full-tuple fast path.
    exec(
        &engine,
        r#"PUT {*tenant: "t1", *doc: "d1", body: "x"} IN "docs""#,
    );
    assert_eq!(
        count(exec(
            &engine,
            r#"SCAN "docs" WHERE tenant = "t1" AND doc = "d1""#
        )),
        1
    );
}

#[test]
fn gravity_by_same_spec_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "x""#);
    exec(&engine, r#"GRAVITY BY (tenant, doc) IN "x""#);
    // Identical re-declaration is a no-op, not an error.
    exec(&engine, r#"GRAVITY BY (tenant, doc) IN "x""#);
}
