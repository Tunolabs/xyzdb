//! Re-gravitation: a SET that changes a record's gravity field physically
//! MOVES the record to its new bucket — the re-gravitation primitive.
//!
//! Without it, a SET on the gravity field leaves the record stranded in its old
//! bucket (its `gravity_hash` no longer matches its placement), invisible to a
//! SCAN by the new gravity value. With it, the move is atomic (one WAL batch:
//! remove old SpatialKey + write new + repoint identity) and the record is
//! found by its new value, gone from the old bucket, with no duplicate.

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

fn note_of(qr: QueryResult) -> Option<String> {
    match qr {
        QueryResult::Records(recs) => recs.first().and_then(|r| match r.fields.get("note") {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        }),
        _ => None,
    }
}

#[test]
fn set_on_gravity_field_moves_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "memory""#);
    exec(
        &engine,
        r#"PUT {*conv:"c1", id:"m0", note:"hola"} IN "memory""#,
    );

    assert_eq!(
        ids(exec(&engine, r#"SCAN "memory" WHERE conv="c1""#)),
        vec!["m0"]
    );
    assert!(ids(exec(&engine, r#"SCAN "memory" WHERE conv="c2""#)).is_empty());

    // Change the gravity field → the record must move to c2's bucket.
    exec(&engine, r#"SET "memory" conv = "c2" WHERE conv = "c1""#);

    assert_eq!(
        ids(exec(&engine, r#"SCAN "memory" WHERE conv="c2""#)),
        vec!["m0"],
        "found via its NEW gravity value (the SCAN fast path resolves the new bucket)"
    );
    assert!(
        ids(exec(&engine, r#"SCAN "memory" WHERE conv="c1""#)).is_empty(),
        "gone from the OLD bucket — not stranded, no stale copy"
    );
    // Exactly one copy across both buckets (no duplicate left behind).
    let total = ids(exec(&engine, r#"SCAN "memory" WHERE conv="c1""#)).len()
        + ids(exec(&engine, r#"SCAN "memory" WHERE conv="c2""#)).len();
    assert_eq!(total, 1, "no duplicate after the move");
    // Other fields survive the move.
    assert_eq!(
        note_of(exec(&engine, r#"SCAN "memory" WHERE conv="c2""#)).as_deref(),
        Some("hola")
    );
}

#[test]
fn set_on_non_gravity_field_stays_put() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "memory""#);
    exec(
        &engine,
        r#"PUT {*conv:"c1", id:"m0", note:"hola"} IN "memory""#,
    );

    // Editing a non-gravity field must NOT move the record.
    exec(
        &engine,
        r#"SET "memory" note = "editado" WHERE conv = "c1""#,
    );

    assert_eq!(
        ids(exec(&engine, r#"SCAN "memory" WHERE conv="c1""#)),
        vec!["m0"]
    );
    assert_eq!(
        note_of(exec(&engine, r#"SCAN "memory" WHERE conv="c1""#)).as_deref(),
        Some("editado")
    );
}
