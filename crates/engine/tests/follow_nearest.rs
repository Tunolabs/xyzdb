//! M3-A: cross-bucket exact `NEAREST` over a `FOLLOW`-resolved union of gravity
//! buckets. `SCAN | FOLLOW | NEAREST` gathers the union of the followed docs'
//! chunks (each document is its own `*doc_id` bucket) and ranks them by embedding
//! similarity — EXACTLY, recall 1.0 over the union, no ANN, no early cut.
//!
//! This is the M3-A contract: the cross-bucket capability already falls out of
//! the generic pipeline (FOLLOW materialises the union, NEAREST scores it). The
//! gate below LOCKS that it is exact — the pipeline's ordered top-k must equal an
//! independent in-test brute-force ranking over the same union, for every k
//! (including k larger than the union).

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

const DIM: usize = 64;

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

/// Ids in RESULT order (top-k order is the property under test — not sorted).
fn ordered_ids(qr: QueryResult) -> Vec<String> {
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

/// Dense DIM-vector with the given (index, value) non-zeros.
fn sparse(pairs: &[(usize, f32)]) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    for &(i, x) in pairs {
        v[i] = x;
    }
    v
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
    format!("[{}]", parts.join(", "))
}

fn cosine_f64(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..a.len() {
        dot += a[i] as f64 * b[i] as f64;
        na += (a[i] as f64) * (a[i] as f64);
        nb += (b[i] as f64) * (b[i] as f64);
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[test]
fn m3a_cross_bucket_nearest_over_follow_union_is_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::open(tmp.path()).unwrap();

    // docs: each document its own *doc_id gravity bucket; chunks carry `emb`.
    exec(&engine, r#"LOBE "docs""#);
    exec(&engine, r#"VECTOR emb IN "docs""#);
    // 3 docs, 5 chunks, each with a DISTINCT cosine to the query so the ranking is
    // unambiguous (no lid-tiebreak dependence): d1a>d1b>d2a>d2b>d3a.
    let chunks: Vec<(&str, &str, Vec<f32>)> = vec![
        ("d1a", "D1", sparse(&[(0, 1.0), (1, 0.1)])),
        ("d1b", "D1", sparse(&[(0, 1.0), (1, 0.3)])),
        ("d2a", "D2", sparse(&[(0, 1.0), (1, 1.0)])),
        ("d2b", "D2", sparse(&[(0, 1.0), (1, 2.0)])),
        ("d3a", "D3", sparse(&[(1, 1.0)])),
    ];
    for (id, doc, emb) in &chunks {
        exec(
            &engine,
            &format!(
                r#"PUT {{*doc_id:"{doc}", id:"{id}", emb:{}}} IN "docs""#,
                vec_literal(emb)
            ),
        );
    }

    // chat: one conversation, each message citing a document (the FOLLOW bridge).
    exec(&engine, r#"LOBE "chat""#);
    for (i, doc) in ["D1", "D2", "D3"].iter().enumerate() {
        exec(
            &engine,
            &format!(r#"PUT {{*conv:"c1", id:"m{i}", doc:"{doc}"}} IN "chat""#),
        );
    }

    let q = sparse(&[(0, 1.0)]);
    // Independent brute-force reference over the WHOLE union (all followed chunks).
    let mut ref_rank: Vec<(&str, f64)> = chunks
        .iter()
        .map(|(id, _, e)| (*id, cosine_f64(&q, e)))
        .collect();
    ref_rank.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let expected: Vec<String> = ref_rank.iter().map(|(id, _)| id.to_string()).collect();

    for k in [1usize, 2, 3, 5, 10] {
        let query = format!(
            r#"SCAN "chat" WHERE conv="c1" | FOLLOW doc TO "docs" ON doc_id | NEAREST(emb, {}, {k}, cosine)"#,
            vec_literal(&q)
        );
        let got = ordered_ids(exec(&engine, &query));
        let want: Vec<String> = expected
            .iter()
            .take(k.min(expected.len()))
            .cloned()
            .collect();
        assert_eq!(
            got, want,
            "cross-bucket NEAREST top-{k} over the FOLLOW union != brute-force"
        );
    }
}
