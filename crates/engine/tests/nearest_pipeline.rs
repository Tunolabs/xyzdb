//! `NEAREST` pipeline step: semantic top-k over a gravity-bounded scan.
//!
//! Uses a tiny 2-D geometry so cosine ordering is obvious by inspection, and
//! checks the load-bearing property: `NEAREST` ranks only the records the
//! preceding `SCAN` returned — i.e. it stays inside the gravity bucket.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

/// Ordered `id` text fields from a record result.
fn ids(qr: QueryResult) -> Vec<String> {
    match qr {
        QueryResult::Records(recs) => recs
            .into_iter()
            .map(|r| match r.fields.get("id") {
                Some(Value::Text(t)) => t.clone(),
                other => panic!("record without id: {other:?}"),
            })
            .collect(),
        other => panic!("expected Records, got {other:?}"),
    }
}

/// Seed two conversation buckets (`*conv`). Bucket `c1` holds the candidates;
/// `c2` holds a record identical to the query to prove gravity bounding.
fn seed(engine: &Engine) {
    exec(engine, r#"LOBE "memoria""#);
    // c1: r1 on the query axis, r2 near it, r3 orthogonal, r4 opposite, r5 no emb.
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"r1", emb:[1.0, 0.0]} IN "memoria""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"r2", emb:[0.9, 0.1]} IN "memoria""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"r3", emb:[0.0, 1.0]} IN "memoria""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"r4", emb:[-1.0, 0.0]} IN "memoria""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"r5", note:"sin embedding"} IN "memoria""#,
    );
    // c2: identical to the query but in a different bucket — must never surface.
    exec(
        engine,
        r#"PUT {*conv:"c2", id:"intruso", emb:[1.0, 0.0]} IN "memoria""#,
    );
}

#[test]
fn cosine_topk_is_bounded_to_the_gravity_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    let qr = exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine)"#,
    );
    // r1 (cos 1.0) then r2 (cos ~0.994). r3/r4 worse, r5 has no emb (skipped),
    // and c2's "intruso" is in another bucket so the scan never sees it.
    assert_eq!(ids(qr), vec!["r1".to_string(), "r2".to_string()]);
}

#[test]
fn k_larger_than_candidates_returns_all_ranked() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // 4 records have an emb in c1; ask for 10. Order by cosine to [1,0]:
    // r1 (1.0) > r2 (~0.994) > r3 (0.0) > r4 (-1.0); r5 skipped (no emb).
    let qr = exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 10, cosine)"#,
    );
    assert_eq!(
        ids(qr),
        vec![
            "r1".to_string(),
            "r2".to_string(),
            "r3".to_string(),
            "r4".to_string()
        ]
    );
}

#[test]
fn l2_orders_by_proximity() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // Query [0.85, 0.15]: unambiguously nearest to r2 [0.9,0.1] (sqdist 0.005)
    // then r1 [1,0] (sqdist 0.045). The margin is ~9x, well clear of any ULP
    // tie — [0.95,0.05] sits exactly equidistant from r1 and r2, so the order
    // there is a floating-point coin-flip (within-ULP kernels may resolve it
    // either way); this query tests proximity ordering, not tie-breaking.
    let qr = exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, [0.85, 0.15], 2, l2)"#,
    );
    assert_eq!(ids(qr), vec!["r2".to_string(), "r1".to_string()]);
}

#[test]
fn unknown_metric_is_a_query_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    let r = engine.run(r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, hamming)"#);
    assert!(r.is_err(), "unknown metric should error, got {r:?}");
}

#[test]
fn bound_param_matches_inline_literal() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // The query vector travels out-of-band as $q — no 768-float literal inline.
    let mut params = std::collections::HashMap::new();
    params.insert(
        "q".to_string(),
        Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
    );
    let qr = engine
        .run_with_params(
            r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, $q, 2, cosine)"#,
            &params,
        )
        .unwrap_or_else(|e| panic!("run_with_params: {e:?}"));
    assert_eq!(ids(qr), vec!["r1".to_string(), "r2".to_string()]);
}

#[test]
fn referencing_an_unbound_param_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // $q used but nothing bound → a clear query error, not silent garbage.
    let r = engine.run_with_params(
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, $q, 2, cosine)"#,
        &std::collections::HashMap::new(),
    );
    assert!(r.is_err(), "unbound param should error, got {r:?}");
}

#[test]
fn ref_uses_a_records_embedding_and_excludes_itself() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // "more like r1": rank the rest by cosine to r1's [1,0] → r2 > r3 > r4;
    // r1 itself is excluded, r5 has no emb.
    let qr = exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, REF "r1", 2, cosine)"#,
    );
    assert_eq!(ids(qr), vec!["r2".to_string(), "r3".to_string()]);
}

#[test]
fn ref_not_in_scope_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    let r = engine.run(r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, REF "nope", 2, cosine)"#);
    assert!(r.is_err(), "REF not in scope should error, got {r:?}");
}

/// P6: the canonical phrase form `NEAREST k BY field TO q [USING m]` is
/// equivalent to the `NEAREST(field, q, k, m)` function alias — same ranking —
/// and `USING` defaults to cosine when omitted.
#[test]
fn phrase_form_equals_function_alias() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    let func = ids(exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine)"#,
    ));
    let phrase = ids(exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST 2 BY emb TO [1.0, 0.0] USING cosine"#,
    ));
    assert_eq!(
        phrase, func,
        "phrase form must rank identically to the function alias"
    );

    // USING omitted → cosine by default → same result again.
    let phrase_default = ids(exec(
        &engine,
        r#"SCAN "memoria" WHERE conv="c1" | NEAREST 2 BY emb TO [1.0, 0.0]"#,
    ));
    assert_eq!(phrase_default, func, "omitted USING must default to cosine");
}
