//! 0.7.6 lightweight ghosts: aggregate ghosts whose per-group rollups
//! live in the dictionary keyspace instead of RAM once cardinality
//! crosses the spill limit.
//!
//! Every test forces a tiny limit via `XYZ_GHOST_SUMMARIES_MAX_GROUPS`
//! (all set the SAME value, so intra-process env races are benign) and
//! verifies the lightweight read path returns exactly what the in-RAM
//! path would: same groups, same counts, same sums — for build-time
//! spills, incremental writes after the flip, and deletes that drain a
//! group to zero.

use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

const SPILL_LIMIT: &str = "4";

fn force_tiny_spill_limit() {
    // SAFETY: set before any engine work in each test; all tests in this
    // binary write the same value, so concurrent setters are idempotent.
    unsafe { std::env::set_var("XYZ_GHOST_SUMMARIES_MAX_GROUPS", SPILL_LIMIT) };
}

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

/// Collect (rfc → (count, sum)) from a grouped aggregation result. Each
/// row carries the group fields plus aggregate columns ("count",
/// "sum(monto)", …) — the single canonical label scheme.
fn grouped(qr: QueryResult) -> std::collections::BTreeMap<String, (i64, f64)> {
    match qr {
        QueryResult::GroupedAggregation(rows) => rows
            .into_iter()
            .map(|row| {
                let gk = match row.get("rfc") {
                    Some(Value::Text(t)) => t.clone(),
                    other => panic!("row without rfc group field: {other:?}"),
                };
                let count = match row.get("count") {
                    Some(Value::Int(c)) => *c,
                    other => panic!("row without count: {other:?}"),
                };
                let sum = row
                    .iter()
                    .find(|(k, _)| k.starts_with("sum("))
                    .and_then(|(_, v)| match v {
                        Value::Float(s) => Some(*s),
                        Value::Int(s) => Some(*s as f64),
                        _ => None,
                    })
                    .unwrap_or(0.0);
                (gk, (count, sum))
            })
            .collect(),
        other => panic!("expected GroupedAggregation, got {other:?}"),
    }
}

#[test]
fn build_spill_preserves_group_aggregates_exactly() {
    force_tiny_spill_limit();
    const GROUPS: usize = 12; // 3× the limit → several spills
    const PER_GROUP: usize = 3;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);

    // Data BEFORE the ghost → exercises the build-path spill.
    for g in 0..GROUPS {
        for k in 0..PER_GROUP {
            exec(
                &engine,
                &format!(
                    r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {m}}} IN "creditos""#,
                    m = 100 * (g + 1) + k
                ),
            );
        }
    }
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );

    // Wildcard: every group present with exact count + sum.
    let all = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(all.len(), GROUPS, "wildcard must return every group");
    for g in 0..GROUPS {
        let gk = format!("RFC{g:03}");
        let expected_sum: f64 = (0..PER_GROUP).map(|k| (100 * (g + 1) + k) as f64).sum();
        let (count, sum) = all
            .get(&gk)
            .unwrap_or_else(|| panic!("group {gk} missing from wildcard read"));
        assert_eq!(*count, PER_GROUP as i64, "count for {gk}");
        assert!(
            (sum - expected_sum).abs() < 1e-9,
            "sum for {gk}: {sum} vs {expected_sum}"
        );
    }

    // Fully pinned: the on-disk point lookup finds exactly one group.
    let one = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "RFC007" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(one.len(), 1, "pinned read must return exactly one group");
    assert_eq!(one.get("RFC007").map(|(c, _)| *c), Some(PER_GROUP as i64));

    // Absent group: empty, not invented.
    let none = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "RFC999" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert!(none.is_empty(), "absent group must return no rows");
}

#[test]
fn incremental_writes_after_flip_stay_consistent() {
    force_tiny_spill_limit();

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);

    // Ghost over an empty lobe (no build groups) — incremental PUTs
    // populate the rollups via notify_write from record one.
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );

    for g in 0..10 {
        for k in 0..2 {
            exec(
                &engine,
                &format!(
                    r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {m}}} IN "creditos""#,
                    m = 10 * (g + 1) + k
                ),
            );
        }
    }

    let all = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        all.len(),
        10,
        "every incrementally-written group must appear"
    );
    assert_eq!(all.get("RFC003").map(|(c, _)| *c), Some(2));

    // Delete one group's records → the group must disappear, others stay.
    exec(&engine, r#"DELETE "creditos" WHERE rfc = "RFC003""#);
    let after = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(after.len(), 9, "drained group must disappear from results");
    assert!(!after.contains_key("RFC003"));
    assert_eq!(
        after.get("RFC004").map(|(c, _)| *c),
        Some(2),
        "other groups untouched"
    );
}

/// The bench's exact lifecycle: ghost created empty, BULKMODE load
/// (aggregates deferred — the per-record rollup RMW collapsed bulk
/// throughput), BULKMODE OFF, REFRESH. The rebuilt ghost must hold the
/// exact aggregates of the loaded data.
#[test]
fn bulk_load_then_refresh_yields_exact_aggregates() {
    force_tiny_spill_limit();

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );

    exec(&engine, "BULKMODE ON");
    const GROUPS: usize = 10;
    const PER_GROUP: usize = 3;
    for g in 0..GROUPS {
        for k in 0..PER_GROUP {
            exec(
                &engine,
                &format!(
                    r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {m}}} IN "creditos""#,
                    m = 10 * (g + 1) + k
                ),
            );
        }
    }
    exec(&engine, "BULKMODE OFF");
    exec(&engine, r#"REFRESH GHOST "credits_by_rfc""#);

    let all = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        all.len(),
        GROUPS,
        "refresh after bulk must rebuild every group"
    );
    for g in 0..GROUPS {
        let gk = format!("RFC{g:03}");
        let expected: f64 = (0..PER_GROUP).map(|k| (10 * (g + 1) + k) as f64).sum();
        let (count, sum) = all.get(&gk).unwrap_or_else(|| panic!("missing {gk}"));
        assert_eq!(*count, PER_GROUP as i64, "count for {gk}");
        assert!(
            (sum - expected).abs() < 1e-9,
            "sum for {gk}: {sum} vs {expected}"
        );
    }
}

#[test]
fn refresh_of_a_lightweight_ghost_rebuilds_cleanly() {
    force_tiny_spill_limit();

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);

    for g in 0..8 {
        exec(
            &engine,
            &format!(r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {g}}} IN "creditos""#),
        );
    }
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );
    // REFRESH = drop + rebuild: old rollups must be purged with the old
    // ghost_id and rebuilt under the new one — stale partials surviving a
    // drop would double-count here.
    exec(&engine, r#"REFRESH GHOST "credits_by_rfc""#);

    let all = grouped(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        all.len(),
        8,
        "rebuilt ghost must hold exactly the source groups"
    );
    for g in 0..8 {
        assert_eq!(
            all.get(&format!("RFC{g:03}")).map(|(c, _)| *c),
            Some(1),
            "group RFC{g:03} must count exactly once after refresh"
        );
    }
}

/// hilo B: rollups are blind delta-appends folded by the merge operator. A
/// COMPACT collapses each group's delta chain; the grouped aggregates must be
/// exact before AND identical after (compaction preserves the fold).
#[test]
fn compact_collapses_rollup_deltas_preserving_aggregates() {
    force_tiny_spill_limit();
    const GROUPS: usize = 12; // 3× the limit → lightweight, multi-delta groups
    const PER_GROUP: usize = 4;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);
    for g in 0..GROUPS {
        for k in 0..PER_GROUP {
            exec(
                &engine,
                &format!(
                    r#"PUT {{_type: "Credit", rfc: "RFC{g:03}", monto: {m}}} IN "creditos""#,
                    m = 100 * (g + 1) + k
                ),
            );
        }
    }
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );

    let q =
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#;
    let before = grouped(exec(&engine, q));
    assert_eq!(before.len(), GROUPS);
    // Exactness spot-check.
    let g = 7usize;
    let expected_sum: f64 = (0..PER_GROUP).map(|k| (100 * (g + 1) + k) as f64).sum();
    assert_eq!(
        before.get(&format!("RFC{g:03}")),
        Some(&(PER_GROUP as i64, expected_sum))
    );

    // Force compaction → the operator folds the per-group delta chains.
    exec(&engine, "COMPACT");

    let after = grouped(exec(&engine, q));
    assert_eq!(after, before, "COMPACT must preserve the folded aggregates");
}
