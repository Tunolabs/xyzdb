//! Teeth for xyTalk v1 P7: DELETE requires WHERE, and PURGE is the explicit
//! total-delete verb. The load-bearing property is consistency AFTER destroying,
//! not just that records vanish: a PURGE must leave ghosts and anchor indexes
//! exact (empty), exactly as a WHERE-matching DELETE of every record would.

// SPDX-License-Identifier: BUSL-1.1
use std::collections::BTreeMap;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

fn put(engine: &Engine, lobe: &str, doc: &str) {
    exec(engine, &format!(r#"PUT {{{doc}}} IN "{lobe}""#));
}

/// Group -> count from the ghost-routed aggregate.
fn counts(engine: &Engine) -> BTreeMap<String, i64> {
    match exec(engine, r#"SCAN "t" | GROUP BY grp | AGGREGATE count()"#) {
        QueryResult::GroupedAggregation(rows) => rows
            .into_iter()
            .map(|r| {
                let g = match r.get("grp") {
                    Some(Value::Text(s)) => s.clone(),
                    other => format!("{other:?}"),
                };
                let c = match r.get("count") {
                    Some(Value::Int(i)) => *i,
                    other => panic!("count missing/non-int: {other:?}"),
                };
                (g, c)
            })
            .collect(),
        other => panic!("expected GroupedAggregation, got {other:?}"),
    }
}

fn scan_count(engine: &Engine, lobe: &str) -> usize {
    match exec(engine, &format!(r#"SCAN "{lobe}" LIMIT 1000"#)) {
        QueryResult::Records(rs) => rs.len(),
        other => panic!("expected Records, got {other:?}"),
    }
}

/// PURGE empties the lobe AND the ghost is exact afterwards (empty), not left
/// carrying pre-purge counts. Consistency after destroying, the whole point.
#[test]
fn purge_empties_lobe_and_ghost_stays_exact() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    for (k, g) in [
        ("k1", "x"),
        ("k2", "x"),
        ("k3", "x"),
        ("k4", "y"),
        ("k5", "y"),
        ("k6", "z"),
    ] {
        put(&e, "t", &format!(r#"_type:"R", k:"{k}", grp:"{g}""#));
    }

    let before = counts(&e);
    assert_eq!(before.get("x"), Some(&3));
    assert_eq!(before.get("y"), Some(&2));
    assert_eq!(before.get("z"), Some(&1));
    assert_eq!(scan_count(&e, "t"), 6);

    exec(&e, r#"PURGE "t""#);

    // Every record gone.
    assert_eq!(scan_count(&e, "t"), 0, "PURGE must empty the lobe");
    // And the ghost is EXACT — no stale groups survive (drift would leave x/y/z).
    let after = counts(&e);
    assert!(
        after.is_empty(),
        "ghost must be empty after PURGE, got {after:?}"
    );
}

/// PURGE clears the anchor index: a FIND by the anchored field finds nothing.
#[test]
fn purge_clears_anchor_index() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    exec(&e, r#"ANCHOR "code" UNIQUE IN "t""#);
    put(&e, "t", r#"_type:"R", code:"A1""#);
    put(&e, "t", r#"_type:"R", code:"A2""#);

    let found_before = match exec(&e, r#"FIND "t" WHERE code = "A1""#) {
        QueryResult::Records(rs) => rs.len(),
        other => panic!("expected Records, got {other:?}"),
    };
    assert_eq!(
        found_before, 1,
        "anchor FIND must locate the record before PURGE"
    );

    exec(&e, r#"PURGE "t""#);

    let found_after = match exec(&e, r#"FIND "t" WHERE code = "A1""#) {
        QueryResult::Records(rs) => rs.len(),
        other => panic!("expected Records, got {other:?}"),
    };
    assert_eq!(found_after, 0, "anchor index must be empty after PURGE");
}

/// The require-WHERE guard reaches the engine, not just the parser: a WHERE-less
/// DELETE errors and teaches PURGE; a WHERE-bearing DELETE still runs.
#[test]
fn delete_without_where_errors_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    put(&e, "t", r#"_type:"R", k:"k1", grp:"x""#);
    put(&e, "t", r#"_type:"R", k:"k2", grp:"y""#);

    let err = e.run(r#"DELETE "t""#).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("WHERE"), "error must mention WHERE: {msg}");
    assert!(msg.contains("PURGE"), "error must teach PURGE: {msg}");
    // Nothing was deleted by the rejected statement.
    assert_eq!(
        scan_count(&e, "t"),
        2,
        "rejected DELETE must not delete anything"
    );

    // A WHERE-bearing DELETE still works.
    exec(&e, r#"DELETE "t" WHERE grp = "x""#);
    assert_eq!(scan_count(&e, "t"), 1);
}
