//! Teeth for xyTalk v1 P1b: SET/DELETE/LINK gain the OR/NOT/IN WHERE tree
//! (resolve_find_expr). Three guards the founder asked for:
//!   1. end-to-end — `DELETE WHERE a OR b` removes A-or-B, not just A;
//!   2. drift — ghosts stay exact after an OR-delete (the new scan+walker write
//!      path still fires `notify_write`), the guard every write path must pass;
//!   3. no-regression — an AND-pure WHERE resolves the same set as before (the
//!      anchor/gravity fast path, taken unchanged when the tree flattens).
//! Plus a SET-by-OR reinforcement (SET is a write-path verb too).

use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

/// Sorted values of a text `field` across all records of a lobe.
fn scan_field(engine: &Engine, lobe: &str, field: &str) -> Vec<String> {
    match exec(engine, &format!(r#"SCAN "{lobe}" LIMIT 1000"#)) {
        QueryResult::Records(rs) => {
            let mut v: Vec<String> = rs
                .iter()
                .filter_map(|r| match r.fields.get(field) {
                    Some(Value::Text(s)) => Some(s.clone()),
                    Some(v) => Some(format!("{v}")),
                    None => None,
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Records, got {other:?}"),
    }
}

fn put(engine: &Engine, lobe: &str, doc: &str) {
    exec(engine, &format!(r#"PUT {{{doc}}} IN "{lobe}""#));
}

/// (1) End-to-end: `DELETE WHERE a OR b` removes both branches, keeps the rest.
#[test]
fn delete_where_or_removes_a_or_b() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    for (k, g) in [("k1", "x"), ("k2", "x"), ("k3", "y"), ("k4", "z")] {
        put(&e, "t", &format!(r#"_type:"R", k:"{k}", grp:"{g}""#));
    }
    exec(&e, r#"DELETE "t" WHERE grp = "x" OR grp = "y""#);
    assert_eq!(
        scan_field(&e, "t", "grp"),
        vec!["z".to_string()],
        "OR-delete must remove x AND y, keep z"
    );
}

/// (2) Drift: a ghost stays exact after an OR-delete — the scan+walker delete
/// path still fires notify_write, so the precomputed aggregate tracks reality.
#[test]
fn ghost_stays_exact_after_or_delete() {
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
    exec(
        &e,
        r#"CREATE GHOST "gc" FROM "t" ORDER BY grp GROUP BY grp AGGREGATE count()"#,
    );

    // Group counts helper reading the ghost-routed aggregate.
    let counts = |e: &Engine| -> std::collections::BTreeMap<String, i64> {
        match exec(e, r#"SCAN "t" | GROUP BY grp | AGGREGATE count()"#) {
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
    };

    // Before: the ghost is live with x:3, y:2, z:1.
    let before = counts(&e);
    assert_eq!(before.get("x"), Some(&3));
    assert_eq!(before.get("y"), Some(&2));
    assert_eq!(before.get("z"), Some(&1));

    // OR-delete removes every x and y (5 records) via scan+walker.
    exec(&e, r#"DELETE "t" WHERE grp = "x" OR grp = "y""#);

    // After: the ghost must be EXACT — only z:1; x/y gone (drift would leave them).
    let after = counts(&e);
    assert_eq!(after.get("z"), Some(&1), "z must survive: {after:?}");
    assert!(
        after.get("x").is_none(),
        "x must be gone from the ghost: {after:?}"
    );
    assert!(
        after.get("y").is_none(),
        "y must be gone from the ghost: {after:?}"
    );
    assert_eq!(after.len(), 1, "only z remains: {after:?}");
}

/// (3) No-regression: an AND-pure WHERE deletes exactly the conjunction (the
/// same set the flat fast path resolved before P1b).
#[test]
fn delete_where_and_pure_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    put(&e, "t", r#"_type:"R", k:"k1", a:1, b:2"#);
    put(&e, "t", r#"_type:"R", k:"k2", a:1, b:3"#);
    put(&e, "t", r#"_type:"R", k:"k3", a:2, b:2"#);
    exec(&e, r#"DELETE "t" WHERE a = 1 AND b = 2"#);
    assert_eq!(
        scan_field(&e, "t", "k"),
        vec!["k2".to_string(), "k3".to_string()],
        "AND-pure must delete only a=1 AND b=2 (k1), leaving k2/k3"
    );
}

/// SET reinforcement: `SET … WHERE a OR b` updates both branches.
#[test]
fn set_where_or_updates_a_or_b() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    for (k, g) in [("k1", "x"), ("k2", "y"), ("k3", "z")] {
        put(
            &e,
            "t",
            &format!(r#"_type:"R", k:"{k}", grp:"{g}", status:"open""#),
        );
    }
    exec(
        &e,
        r#"SET "t" status = "done" WHERE grp = "x" OR grp = "y""#,
    );
    let mut done: Vec<String> = match exec(&e, r#"SCAN "t" LIMIT 1000"#) {
        QueryResult::Records(rs) => rs
            .iter()
            .filter(|r| matches!(r.fields.get("status"), Some(Value::Text(s)) if s == "done"))
            .filter_map(|r| match r.fields.get("grp") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        other => panic!("expected Records, got {other:?}"),
    };
    done.sort();
    assert_eq!(
        done,
        vec!["x".to_string(), "y".to_string()],
        "SET OR must update x and y, not z"
    );
}

/// P1 (SCAN GHOST): the ghost read gains the tree. OR returns the union
/// (read + walker-filter), AND-pure takes the read_topn pushdown, no WHERE
/// returns all — same records either way.
#[test]
fn scan_ghost_where_or_returns_union() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "t""#);
    for (k, g) in [("k1", "x"), ("k2", "x"), ("k3", "y"), ("k4", "z")] {
        put(&e, "t", &format!(r#"_type:"R", k:"{k}", grp:"{g}""#));
    }
    exec(&e, r#"CREATE GHOST "g" FROM "t" ORDER BY grp EMBED grp, k"#);
    let grps = |q: &str| -> Vec<String> {
        match exec(&e, q) {
            QueryResult::Records(rs) => {
                let mut v: Vec<String> = rs
                    .iter()
                    .filter_map(|r| match r.fields.get("grp") {
                        Some(Value::Text(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                v.sort();
                v
            }
            other => panic!("expected Records, got {other:?}"),
        }
    };
    // OR → union of x and y (k1,k2 grp x + k3 grp y); z excluded.
    assert_eq!(
        grps(r#"SCAN GHOST "g" WHERE grp = "x" OR grp = "y""#),
        vec!["x".to_string(), "x".to_string(), "y".to_string()],
        "SCAN GHOST OR must return the union x+y"
    );
    // AND-pure → read_topn pushdown, only x.
    assert_eq!(
        grps(r#"SCAN GHOST "g" WHERE grp = "x""#),
        vec!["x".to_string(), "x".to_string()]
    );
    // No WHERE → all four records.
    assert_eq!(grps(r#"SCAN GHOST "g""#).len(), 4);
}
