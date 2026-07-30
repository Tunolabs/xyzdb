//! Repro for the Q5OverdueByEmpresa empty-result regression: an explicit
//! `SCAN GHOST` over a filtered GROUP BY / AGGREGATE ghost must return one
//! aggregate row per group, not empty. Mirrors the bench ghost shape
//! (`WHERE _type=X AND state=Y GROUP BY g AGGREGATE sum(a), count()`) with
//! domain-neutral field names.

use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    let stmt = xytalk_parser::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e:?}"));
    engine
        .execute(stmt)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn count(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("unexpected scan result: {other:?}"),
    }
}

#[test]
fn scan_ghost_on_filtered_grouped_aggregate_is_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#);

    // Matching records: _type="inst" AND state="overdue", across two groups.
    for (g, amt) in [("E1", 100.0_f64), ("E1", 50.0), ("E2", 200.0)] {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type: "inst", state: "overdue", grp: "{g}", amount: {amt}}} IN "items""#
            ),
        );
    }
    // Non-matching rows the filter must exclude.
    exec(
        &engine,
        r#"PUT {_type: "inst", state: "active", grp: "E1", amount: 999} IN "items""#,
    );
    exec(
        &engine,
        r#"PUT {_type: "pay", state: "overdue", grp: "E1", amount: 999} IN "items""#,
    );

    // Q5-shape ghost: two-predicate filter + GROUP BY + AGGREGATE.
    exec(
        &engine,
        r#"CREATE GHOST "overdue_by_grp" FROM "items" WHERE _type = "inst" AND state = "overdue" ORDER BY grp GROUP BY grp AGGREGATE sum(amount), count()"#,
    );

    // Q5's exact path: explicit ghost scan. Must yield one row per group
    // (E1, E2) — pre-regression behaviour. Empty = the bug.
    let n = count(exec(&engine, r#"SCAN GHOST "overdue_by_grp""#));
    assert_eq!(
        n, 2,
        "SCAN GHOST on a filtered grouped-aggregate ghost must return one row per group (E1, E2), got {n}"
    );
}

/// The bench's exact lifecycle for Q5: the aggregate ghost is created, more
/// matching records arrive (aggregate ghosts are NOT incrementally folded), and
/// a REFRESH rebuilds it from the records. SCAN GHOST must then reflect every
/// group. Exercises the streaming `create` path that REFRESH (drop + rebuild)
/// reuses — the path that OOMed at scale before the fix.
#[test]
fn refresh_repopulates_aggregate_ghost_from_all_records() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "items""#);

    // Records present before the ghost exists.
    for (g, amt) in [("E1", 100.0_f64), ("E2", 200.0)] {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type: "inst", state: "overdue", grp: "{g}", amount: {amt}}} IN "items""#
            ),
        );
    }
    exec(
        &engine,
        r#"CREATE GHOST "g2" FROM "items" WHERE _type = "inst" AND state = "overdue" ORDER BY grp GROUP BY grp AGGREGATE sum(amount), count()"#,
    );

    // More matching records after creation, in a new group.
    for (g, amt) in [("E1", 50.0_f64), ("E3", 300.0)] {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type: "inst", state: "overdue", grp: "{g}", amount: {amt}}} IN "items""#
            ),
        );
    }

    // REFRESH = drop + rebuild from all records via the streaming create path.
    exec(&engine, r#"REFRESH GHOST "g2""#);

    let n = count(exec(&engine, r#"SCAN GHOST "g2""#));
    assert_eq!(
        n, 3,
        "after REFRESH the aggregate ghost must cover all groups (E1, E2, E3), got {n}"
    );
}
