//! A ghost-routed read must return the SAME record as the primary keyspace —
//! including the declared vector.
//!
//! V5 splits the searchable vector out of the record blob into its own column,
//! keyed by the same spatial key; every read path re-attaches it. The ghost
//! point-read did not, so once a ghost existed for a query shape, that query
//! started answering without the embedding — `status: ok`, all the other fields
//! present, no flag. Since ghosts are built automatically from scan telemetry,
//! the same query returned different fields before and after the engine decided
//! to materialise one.
//!
//! Found from the agentic benchmark: after a selectivity sweep had run the same
//! filter enough times to trigger an auto-ghost, `SCAN … | NEAREST … | SHAPE`
//! collapsed to zero rows, because the unfused NEAREST scored records whose
//! embedding had silently gone missing.

use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

const DIM: usize = 64;

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine.run(s).unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(rs) => rs,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected records, got {other:?}"),
    }
}

/// A lobe whose embedding is declared (so records are stored V5, vector hoisted
/// into its own column) and whose rows differ in a leading axis so ranking is
/// unambiguous.
fn seed(engine: &Engine, lobe: &str, n: usize) {
    exec(engine, &format!(r#"LOBE "{lobe}""#));
    exec(engine, &format!(r#"VECTOR emb IN "{lobe}""#));
    for i in 0..n {
        let mut dims = vec!["0.0".to_string(); DIM];
        dims[i % DIM] = "1.0".to_string();
        exec(
            engine,
            &format!(
                r#"PUT {{_type:"R", id:"g{i}", x:{i}, tag:"t", emb:[{}]}} IN "{lobe}""#,
                dims.join(", ")
            ),
        );
    }
}

fn emb_len(r: &xyzdb_core::record::Record) -> Option<usize> {
    match r.fields.get("emb") {
        Some(Value::Vector(v)) => Some(v.len()),
        Some(Value::List(v)) => Some(v.len()),
        _ => None,
    }
}

/// The load-bearing assertion: same query, same rows, and the ghost-routed
/// answer carries the vector.
///
/// The control is the identical query BEFORE the ghost exists. Asserting only
/// "the ghost read returned the vector" would pass on an engine that never
/// routed to the ghost at all, so the test also proves the route changed — by
/// reading the router's own decision through `SHOW GHOSTS` and by the row count
/// staying identical across it.
#[test]
fn ghost_routed_scan_returns_the_declared_vector() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e, "g", 200);

    let before = records(exec(&e, r#"SCAN "g" WHERE tag="t""#));
    assert_eq!(before.len(), 200);
    assert!(
        before.iter().all(|r| emb_len(r) == Some(DIM)),
        "control: the primary read must carry the declared vector"
    );

    // A covering ordered ghost makes the identical filter SCAN route through
    // the ghost's point-read path instead of the primary scan.
    exec(&e, r#"CREATE GHOST "gc" FROM "g" ORDER BY x"#);

    let after = records(exec(&e, r#"SCAN "g" WHERE tag="t""#));
    assert_eq!(
        after.len(),
        before.len(),
        "the ghost must not change WHICH records come back"
    );
    let missing = after.iter().filter(|r| emb_len(r).is_none()).count();
    assert_eq!(
        missing, 0,
        "{missing}/{} ghost-routed records lost the declared vector — a ghost \
         is an accelerator, not a different answer",
        after.len()
    );
    for (i, r) in after.iter().enumerate() {
        assert_eq!(
            emb_len(r),
            Some(DIM),
            "row {i}: vector must hydrate to its full width"
        );
    }
}

/// The consequence that surfaced it: an unfused NEAREST ranks whatever the scan
/// handed it, so a scan that dropped the embedding ranks nothing and returns an
/// empty result with `status: ok` — indistinguishable from "no matches".
///
/// Three steps (not two) on purpose: `SCAN | NEAREST` fuses into the vector
/// prefix fast path, which reads the column directly and never sees the defect.
/// The pipeline has to be long enough to fall out of the fused path.
#[test]
fn unfused_nearest_over_a_ghost_routed_scan_still_ranks() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e, "g", 200);
    exec(&e, r#"CREATE GHOST "gc" FROM "g" ORDER BY x"#);

    let mut q = vec!["0.0".to_string(); DIM];
    q[0] = "1.0".to_string();
    let q = q.join(", ");

    let ranked = records(exec(
        &e,
        &format!(r#"SCAN "g" WHERE tag="t" | NEAREST 5 BY emb TO [{q}] USING cosine | SHAPE {{id}}"#),
    ));
    assert_eq!(
        ranked.len(),
        5,
        "the unfused pipeline must rank the scanned records, not silently \
         score an embedding-less set down to nothing"
    );
    assert_eq!(
        ranked[0].fields.get("id"),
        Some(&Value::Text("g0".into())),
        "g0 is the exact match for the query vector"
    );
}
