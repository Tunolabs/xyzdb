//! `FOLLOW` — cross-entity (cross-bucket) expansion, the relational layer of
//! context assembly that PULL cannot reach.
//!
//! Two lobes with DIFFERENT gravity: `chat` (gravity `*conv`) holds messages
//! that reference a document by id; `docs` (gravity `*doc_id`) holds the
//! documents. `... | NEAREST | FOLLOW doc TO "docs" ON doc_id` crosses from the
//! conversation's relevant messages to their cited documents — a different
//! entity in a different bucket — in one pipeline. This is the bridge that
//! makes associative context span entities, not just one conversation.

// SPDX-License-Identifier: BUSL-1.1
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

fn seed(engine: &Engine) {
    // docs: gravity *doc_id (each document its own bucket; chunks co-located).
    exec(engine, r#"LOBE "docs""#);
    exec(
        engine,
        r#"PUT {*doc_id:"D1", id:"d1a", text:"gripe: tratamiento"} IN "docs""#,
    );
    exec(
        engine,
        r#"PUT {*doc_id:"D1", id:"d1b", text:"gripe: dosis"} IN "docs""#,
    );
    exec(
        engine,
        r#"PUT {*doc_id:"D2", id:"d2a", text:"política de facturación"} IN "docs""#,
    );
    // chat: gravity *conv; messages carry an embedding + a `doc` reference.
    exec(engine, r#"LOBE "chat""#);
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m0", emb:[1.0, 0.0], doc:"D1"} IN "chat""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m1", emb:[0.9, 0.1], doc:"D1"} IN "chat""#,
    );
    exec(
        engine,
        r#"PUT {*conv:"c1", id:"m2", emb:[0.0, 1.0], doc:"D2"} IN "chat""#,
    );
}

#[test]
fn follow_crosses_from_chat_to_cited_documents() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // The 2 relevant messages (m0, m1) both cite D1 → FOLLOW fetches D1's chunks
    // from the `docs` lobe (a different gravity bucket). D2 is not cited by the
    // top-2, so it must not appear. The reference value is followed once (dedup).
    let r = exec(
        &engine,
        r#"SCAN "chat" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 2, cosine) | FOLLOW doc TO "docs" ON doc_id"#,
    );
    assert_eq!(ids(r), vec!["d1a".to_string(), "d1b".to_string()]);
}

#[test]
fn follow_gathers_multiple_distinct_references() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // Top-3 covers m0/m1 (D1) and m2 (D2) → both documents' chunks, deduped.
    let r = exec(
        &engine,
        r#"SCAN "chat" WHERE conv="c1" | NEAREST(emb, [1.0, 0.0], 3, cosine) | FOLLOW doc TO "docs" ON doc_id"#,
    );
    assert_eq!(
        ids(r),
        vec!["d1a".to_string(), "d1b".to_string(), "d2a".to_string()]
    );
}

#[test]
fn follow_standalone_after_scan() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine);

    // FOLLOW also composes without NEAREST: every c1 message → its cited docs.
    let r = exec(
        &engine,
        r#"SCAN "chat" WHERE conv="c1" | FOLLOW doc TO "docs" ON doc_id"#,
    );
    assert_eq!(
        ids(r),
        vec!["d1a".to_string(), "d1b".to_string(), "d2a".to_string()]
    );
}
