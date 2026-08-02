//! Teeth for xyTalk v1 P9: `| SHAPE {f1, f2}` projects each record to the named
//! fields. It is a projection, not a filter: it must return exactly the named
//! fields present on a record (no more, no fewer), leave the rest of the query
//! (WHERE, ORDER BY, LIMIT — which records, how many, in what order) untouched,
//! and still return records that lack a named field (just without it).

use std::collections::BTreeSet;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(rs) => rs,
        other => panic!("expected Records, got {other:?}"),
    }
}

fn field_keys(r: &xyzdb_core::record::Record) -> BTreeSet<String> {
    r.fields.keys().cloned().collect()
}

fn seed(engine: &Engine) {
    exec(engine, r#"LOBE "l""#);
    // Each record has k, grp, val, extra, plus the auto _type field.
    for (k, grp, val) in [("k1", "x", 30), ("k2", "x", 10), ("k3", "y", 20)] {
        exec(
            engine,
            &format!(r#"PUT {{_type:"R", k:"{k}", grp:"{grp}", val:{val}, extra:"e"}} IN "l""#),
        );
    }
}

/// SHAPE returns exactly the named fields that exist — no more (extra/val/_type
/// dropped), no fewer.
#[test]
fn shape_returns_exactly_named_fields() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e);

    let shaped = records(exec(&e, r#"SCAN "l" LIMIT 100 | SHAPE {k, grp}"#));
    assert_eq!(shaped.len(), 3, "SHAPE must not drop records");
    let want: BTreeSet<String> = ["k", "grp"].iter().map(|s| s.to_string()).collect();
    for r in &shaped {
        assert_eq!(
            field_keys(r),
            want,
            "SHAPE {{k, grp}} must yield exactly k and grp (val/extra/_type dropped)"
        );
    }
}

/// SHAPE leaves the rest of the query intact: same records, same count, same
/// order as the identical query without SHAPE (compared by structural LID).
#[test]
fn shape_leaves_filter_order_limit_intact() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e);

    let base = records(exec(
        &e,
        r#"SCAN "l" WHERE grp = "x" ORDER BY val DESC LIMIT 5"#,
    ));
    let shaped = records(exec(
        &e,
        r#"SCAN "l" WHERE grp = "x" ORDER BY val DESC LIMIT 5 | SHAPE {k}"#,
    ));

    let base_lids: Vec<_> = base.iter().map(|r| r.lid).collect();
    let shaped_lids: Vec<_> = shaped.iter().map(|r| r.lid).collect();
    assert_eq!(
        shaped_lids, base_lids,
        "SHAPE must not change which records, how many, or their order"
    );
    // WHERE grp="x" → k1(30) then k2(10) by val DESC.
    let ks: Vec<String> = shaped
        .iter()
        .map(|r| match r.fields.get("k") {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("k missing after SHAPE: {other:?}"),
        })
        .collect();
    assert_eq!(ks, vec!["k1".to_string(), "k2".to_string()]);
}

/// Projection, not filter: a record lacking a named field is still returned,
/// just without that field.
#[test]
fn shape_is_projection_not_filter() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "l""#);
    exec(&e, r#"PUT {_type:"R", k:"has", grp:"x"} IN "l""#);
    exec(&e, r#"PUT {_type:"R", k:"lacks"} IN "l""#); // no grp

    let shaped = records(exec(&e, r#"SCAN "l" LIMIT 100 | SHAPE {k, grp}"#));
    assert_eq!(shaped.len(), 2, "the record lacking grp must still appear");
    for r in &shaped {
        match r.fields.get("k") {
            Some(Value::Text(s)) if s == "has" => {
                assert!(r.fields.contains_key("grp"), "'has' should keep grp");
            }
            Some(Value::Text(s)) if s == "lacks" => {
                assert!(
                    !r.fields.contains_key("grp"),
                    "'lacks' has no grp — SHAPE must not invent it"
                );
                let want: BTreeSet<String> = ["k"].iter().map(|s| s.to_string()).collect();
                assert_eq!(field_keys(r), want, "'lacks' shapes down to just k");
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }
}

/// SHAPE composed AFTER NEAREST must project the ranked rows, not erase them.
///
/// Found from the agentic benchmark while trying to project the 1024-float
/// embedding out of the response. `SCAN | NEAREST` (two steps) takes the fused
/// fast path; adding `| SHAPE` makes it three steps, so it falls to the generic
/// loop — a different code path.
///
/// The lobe declares `VECTOR emb` and the width is >= `VECTOR_F32_MIN_DIMS`, so
/// the embedding is stored HOISTED out of the record body. A first version of
/// this test used a 2-d undeclared list and passed while the bench failed: with
/// no declaration the record stays V1 and `emb` is an ordinary field, so the
/// generic loop never had to find a hoisted column. The declaration is the
/// variable under test, not decoration.
#[test]
fn shape_after_nearest_projects_and_keeps_rows() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "v""#);
    exec(&e, r#"GRAVITY BY bucket IN "v""#);
    exec(&e, r#"SATELLITE BY cat2 IN "v""#);
    exec(&e, r#"VECTOR emb IN "v""#);
    // 64 dims = VECTOR_F32_MIN_DIMS, the width above which a float list is
    // packed and hoisted. Each record leans on a different leading axis so the
    // ranking is unambiguous.
    for (i, k) in ["a", "b", "c"].iter().enumerate() {
        let mut dims = vec!["0.0".to_string(); 64];
        dims[i] = "1.0".to_string();
        exec(
            &e,
            &format!(
                r#"PUT {{_type:"R", k:"{k}", bucket:"0", cat2:1, emb:[{}]}} IN "v""#,
                dims.join(", ")
            ),
        );
    }
    let mut q = vec!["0.0".to_string(); 64];
    q[0] = "1.0".to_string();
    let q = q.join(", ");

    let plain = records(exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" AND cat2 = 1 | NEAREST 2 BY emb TO [{q}] USING cosine"#
        ),
    ));
    assert_eq!(plain.len(), 2, "control: NEAREST alone returns its top-2");

    let shaped = records(exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" AND cat2 = 1 | NEAREST 2 BY emb TO [{q}] USING cosine | SHAPE {{k}}"#
        ),
    ));
    assert_eq!(
        shaped.len(),
        plain.len(),
        "SHAPE is a projection: it must not drop the ranked rows"
    );
    let want: BTreeSet<String> = ["k"].iter().map(|s| s.to_string()).collect();
    for (i, r) in shaped.iter().enumerate() {
        assert_eq!(field_keys(r), want, "row {i} must shape down to just k");
        assert_eq!(
            r.lid, plain[i].lid,
            "row {i}: SHAPE must preserve which rows and their order"
        );
    }
}
