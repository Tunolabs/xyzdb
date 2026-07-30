//! Context assembly in one read — the "vector gravity for context" primitive.
//!
//! The associative-memory pattern is NOT a monolithic command: it composes from
//! existing primitives. `SCAN <scope> | NEAREST(emb, q, k) | PULL` is, in one
//! gravity-bounded read:
//!   - structural focus  (SCAN WHERE = the scope/bucket),
//!   - semantic focus    (NEAREST = the k most relevant within the scope),
//!   - relational expand (PULL = each hit's co-located neighbourhood).
//!
//! These tests lock that composition as a contract so a future change to
//! NEAREST or PULL cannot silently break context assembly. Deterministic 2-D
//! embeddings (no external embedder).
//!
//! Known boundary (the next engine layer, NOT this primitive): PULL expands
//! within the gravity bucket; following a reference to a DIFFERENT entity's
//! bucket (chat → its cited document) needs the relational layer (LINK-follow /
//! satellite gravity).

use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn ids(qr: QueryResult) -> Vec<String> {
    match qr {
        QueryResult::Records(recs) => {
            let mut v: Vec<String> = recs
                .into_iter()
                .map(|r| match r.fields.get("id") {
                    Some(Value::Text(t)) => t.clone(),
                    other => panic!("record without id: {other:?}"),
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Records, got {other:?}"),
    }
}

/// Two conversations (gravity `*conv`). c1 is the focus; c2 holds a record
/// identical to the query to prove the whole pipeline stays gravity-bounded.
fn seed(engine: &Engine) {
    exec(engine, r#"LOBE "memory""#);
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m0", emb:[1.0, 0.0]} IN "memory""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m1", emb:[0.9, 0.1]} IN "memory""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m2", emb:[0.0, 1.0]} IN "memory""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m3", emb:[-1.0, 0.0]} IN "memory""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c2", id:"x0", emb:[1.0, 0.0]} IN "memory""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c2", id:"x1", emb:[0.5, 0.5]} IN "memory""#,
    );
}

#[test]
fn nearest_then_pull_assembles_the_scope_context() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // Semantic focus: the 2 nearest to [1,0] within c1 → m0, m1.
    let focus = exec(
        &engine,
        r#"SCAN "memory" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine)"#,
    );
    assert_eq!(ids(focus), vec!["m0".to_string(), "m1".to_string()]);

    // Relational expand: PULL re-expands the hits to their co-located context
    // (the whole c1 conversation) — focus + surrounding context in one read.
    let assembled = exec(
        &engine,
        r#"SCAN "memory" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine) | PULL"#,
    );
    assert_eq!(
        ids(assembled),
        vec![
            "m0".to_string(),
            "m1".to_string(),
            "m2".to_string(),
            "m3".to_string()
        ],
        "PULL should expand the semantic hits to the full c1 bucket"
    );
}

#[test]
fn assembly_stays_gravity_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // c2 holds x0 == the query vector, but the whole NEAREST|PULL pipeline is
    // scoped to c1 — no c2 record may surface at any stage.
    let assembled = exec(
        &engine,
        r#"SCAN "memory" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine) | PULL"#,
    );
    let got = ids(assembled);
    assert!(
        got.iter().all(|id| id.starts_with('m')),
        "no c2 (x*) record may leak into the assembled context, got {got:?}"
    );
}

#[test]
fn focus_expand_then_rerank_by_relevance() {
    // The pattern a RAG caller actually wants: focus → expand → RE-RANK the
    // expanded context by relevance, keep the top few. Pure composition:
    // NEAREST | PULL | NEAREST. From {m0,m1,m2,m3} the top-3 by
    // cosine to [1,0] are m0 (1.0), m1 (~.99), m2 (0) — m3 (-1) drops.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    let ranked = exec(
        &engine,
        r#"SCAN "memory" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine) | PULL | NEAREST(emb, [1.0, 0.0], 3, cosine)"#,
    );
    assert_eq!(
        ids(ranked),
        vec!["m0".to_string(), "m1".to_string(), "m2".to_string()],
        "re-rank the expanded context by relevance, drop the least relevant"
    );
}
