//! Teeth for xyTalk v1 P2: `TAKE` is the canonical top-N / truncate step and
//! `TOP` is a live alias. The equivalence is LOAD-BEARING for the native
//! benchmark harness, whose Q4 driver ships `… | TOP n BY sum(monto)` — that
//! spelling must keep producing byte-identical results, and the canonical
//! `TAKE` spelling must match it exactly.
//!
//! Guards:
//!   1. TOP ≡ TAKE on a runtime aggregate (both directions of BY);
//!   2. TOP ≡ TAKE on a metric-ordered ghost (the O(N) fast path the driver
//!      relies on) — the alias must not miss the optimization;
//!   3. `TAKE n` without BY truncates grouped rows (pipeline LIMIT), no reorder;
//!   4. `SCAN | TAKE n` without BY truncates a plain record stream.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

/// Ordered (group, sum) pairs from a GroupedAggregation result.
fn grouped(qr: QueryResult) -> Vec<(String, f64)> {
    let rows = match qr {
        QueryResult::GroupedAggregation(v) => v,
        other => panic!("expected grouped aggregation, got {other:?}"),
    };
    rows.into_iter()
        .map(|m| {
            let grp = match m.get("grp") {
                Some(Value::Text(s)) => s.clone(),
                Some(v) => format!("{v}"),
                None => String::new(),
            };
            let sum = match m.get("sum(amount)") {
                Some(Value::Float(f)) => *f,
                Some(Value::Int(i)) => *i as f64,
                other => panic!("sum(amount) missing/non-numeric: {other:?}"),
            };
            (grp, sum)
        })
        .collect()
}

fn put_group(engine: &Engine, lobe: &str, g: usize, amt: i64) {
    exec(
        engine,
        &format!(r#"PUT {{_type:"R", grp:"g{g:02}", amount:{amt}}} IN "{lobe}""#),
    );
}

/// (1) + (2): `TOP` and `TAKE` are byte-identical, on both the runtime path and
/// the metric-ordered ghost O(N) path, DESC (default) and ASC.
#[test]
fn take_equals_top_bit_identical() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "rt""#); // runtime, no ghost
    exec(&engine, r#"LOBE "gh""#); // metric-ordered ghost
    for g in 0..12 {
        // Two records per group so the ghost rollup genuinely aggregates.
        put_group(&engine, "rt", g, ((g + 1) * 10) as i64);
        put_group(&engine, "rt", g, 5);
        put_group(&engine, "gh", g, ((g + 1) * 10) as i64);
        put_group(&engine, "gh", g, 5);
    }
    // Ghost ordered by the metric → `TAKE n BY sum(amount)` rides the O(N) path.
    exec(
        &engine,
        r#"CREATE GHOST "g4" FROM "gh" ORDER BY sum(amount) GROUP BY grp AGGREGATE sum(amount)"#,
    );

    let q = |verb: &str, lobe: &str, tail: &str| -> Vec<(String, f64)> {
        grouped(exec(
            &engine,
            &format!(
                r#"SCAN "{lobe}" | GROUP BY grp | AGGREGATE sum(amount) | {verb} 5 BY sum(amount){tail}"#
            ),
        ))
    };

    for (lobe, what) in [("rt", "runtime"), ("gh", "metric-order ghost")] {
        // Default direction (DESC).
        assert_eq!(
            q("TOP", lobe, ""),
            q("TAKE", lobe, ""),
            "TOP != TAKE on {what} (default DESC)"
        );
        // Explicit ASC — the alias must carry the direction too.
        assert_eq!(
            q("TOP", lobe, " ASC"),
            q("TAKE", lobe, " ASC"),
            "TOP != TAKE on {what} (ASC)"
        );
        // And the alias must not silently drop the metric ordering.
        assert_eq!(
            q("TAKE", lobe, "").len(),
            5,
            "TAKE 5 must return 5 on {what}"
        );
    }
}

/// (3): `TAKE n` without BY truncates the grouped rows to the first n, no reorder.
#[test]
fn take_without_by_truncates_grouped() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    for g in 0..10 {
        put_group(&engine, "t", g, ((g + 1) * 10) as i64);
    }
    let all = grouped(exec(
        &engine,
        r#"SCAN "t" | GROUP BY grp | AGGREGATE sum(amount)"#,
    ));
    let cut = grouped(exec(
        &engine,
        r#"SCAN "t" | GROUP BY grp | AGGREGATE sum(amount) | TAKE 4"#,
    ));
    assert_eq!(cut.len(), 4, "TAKE 4 must keep 4 groups");
    assert_eq!(
        cut,
        all[..4].to_vec(),
        "TAKE n = first n of the stream, no reorder"
    );
}

/// (4): `SCAN | TAKE n` without BY truncates a plain record stream (pipeline LIMIT).
#[test]
fn take_without_by_truncates_records() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    for g in 0..8 {
        put_group(&engine, "t", g, 1);
    }
    let n = match exec(&engine, r#"SCAN "t" | TAKE 3"#) {
        QueryResult::Records(rs) => rs.len(),
        other => panic!("expected records, got {other:?}"),
    };
    assert_eq!(n, 3, "SCAN | TAKE 3 must truncate the record stream to 3");
}
