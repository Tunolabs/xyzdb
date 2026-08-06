//! Teeth for the metric-ordered ghost (`CREATE GHOST … ORDER BY <metric>`): the
//! O(N) `TOP n BY <metric>` served from the metric-ordered rollup is
//! bit-identical to the O(M) quickselect over the same groups — ties at the
//! N/N+1 cut and M<N included. Also verifies the freshness/fallback contract:
//! a TOP by a *different* metric than the declared order falls back to O(M)
//! (still correct), and SHOW GHOSTS surfaces the order + its emitted age.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

fn info(qr: QueryResult) -> Vec<String> {
    match qr {
        QueryResult::Info(v) => v,
        other => panic!("expected info, got {other:?}"),
    }
}

/// Ordered (group, metric) pairs from a GroupedAggregation, reading `label`.
fn rows(qr: QueryResult, label: &str) -> Vec<(String, f64)> {
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
            let metric = match m.get(label) {
                Some(Value::Float(f)) => *f,
                Some(Value::Int(i)) => *i as f64,
                other => panic!("{label} missing/non-numeric: {other:?}"),
            };
            (grp, metric)
        })
        .collect()
}

/// Oracle: all groups under the same total order (metric by `desc`, then group
/// key ascending), truncated to n. What the O(N) served TOP must reproduce.
fn oracle(mut all: Vec<(String, f64)>, n: usize, desc: bool) -> Vec<(String, f64)> {
    all.sort_by(|a, b| {
        let primary = if desc {
            b.1.total_cmp(&a.1)
        } else {
            a.1.total_cmp(&b.1)
        };
        primary.then_with(|| a.0.cmp(&b.0))
    });
    all.truncate(n);
    all
}

fn put_group(engine: &Engine, lobe: &str, g: usize, amt: i64) {
    exec(
        engine,
        &format!(r#"PUT {{_type:"R", grp:"g{g:02}", amount:{amt}}} IN "{lobe}""#),
    );
}

/// A ghost that keeps its groups ordered by `sum(amount)` (so TOP is O(N)).
fn create_ordered_ghost(engine: &Engine, lobe: &str, name: &str, desc: bool) {
    let dir = if desc { "DESC" } else { "ASC" };
    exec(
        engine,
        &format!(
            r#"CREATE GHOST "{name}" FROM "{lobe}" ORDER BY sum(amount) {dir} GROUP BY grp AGGREGATE sum(amount), count()"#
        ),
    );
}

fn top(engine: &Engine, lobe: &str, n: usize, by: &str, dir: &str) -> Vec<(String, f64)> {
    let label = format!("{by}");
    rows(
        exec(
            engine,
            &format!(
                r#"SCAN "{lobe}" | GROUP BY grp | AGGREGATE sum(amount), count() | TOP {n} BY {by} {dir}"#
            ),
        ),
        &label,
    )
}

fn all(engine: &Engine, lobe: &str, label: &str) -> Vec<(String, f64)> {
    rows(
        exec(
            engine,
            &format!(r#"SCAN "{lobe}" | GROUP BY grp | AGGREGATE sum(amount), count()"#),
        ),
        label,
    )
}

/// The whole point: the O(N) order path equals the O(M) quickselect path, and
/// both equal sort-all-then-truncate. `ord` is served from the metric-ordered
/// rollup; `plain` (no ghost) is the runtime O(M) quickselect.
#[test]
fn order_topn_equals_quickselect_and_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "ord""#);
    exec(&engine, r#"LOBE "plain""#);
    for g in 0..20 {
        // Two records per group so the rollup actually aggregates.
        let a = ((g + 1) * 10) as i64;
        put_group(&engine, "ord", g, a);
        put_group(&engine, "ord", g, 5);
        put_group(&engine, "plain", g, a);
        put_group(&engine, "plain", g, 5);
    }
    create_ordered_ghost(&engine, "ord", "g_ord", true);

    // The order must be emitted (else the O(N) path silently falls back to O(M)
    // and this test would be vacuous).
    let shown = info(exec(&engine, "SHOW GHOSTS")).join("\n");
    assert!(
        shown.contains("metric-order sum(amount) DESC") && shown.contains("emitted"),
        "SHOW GHOSTS must report the emitted order, got:\n{shown}"
    );

    for n in [1usize, 3, 10, 20] {
        let served = top(&engine, "ord", n, "sum(amount)", "DESC");
        let quickselect = top(&engine, "plain", n, "sum(amount)", "DESC");
        let want = oracle(all(&engine, "ord", "sum(amount)"), n, true);
        assert_eq!(
            served, quickselect,
            "O(N) order != O(M) quickselect (n={n})"
        );
        assert_eq!(served, want, "O(N) order != sort-all+truncate (n={n})");
        assert_eq!(served.len(), n.min(20));
    }
}

/// Ties at the N/N+1 cut are broken identically (group key ascending).
#[test]
fn order_ties_at_cut_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    put_group(&engine, "t", 0, 100);
    put_group(&engine, "t", 1, 50);
    put_group(&engine, "t", 2, 50);
    put_group(&engine, "t", 3, 50);
    put_group(&engine, "t", 4, 10);
    create_ordered_ghost(&engine, "t", "g_t", true);
    let served = top(&engine, "t", 2, "sum(amount)", "DESC");
    assert_eq!(
        served,
        vec![("g00".to_string(), 100.0), ("g01".to_string(), 50.0)],
        "tie at cut must break by group key ascending"
    );
    assert_eq!(served, oracle(all(&engine, "t", "sum(amount)"), 2, true));
    assert_eq!(served, top(&engine, "t", 2, "sum(amount)", "DESC")); // repeatable
}

/// M < N returns all M groups.
#[test]
fn order_fewer_groups_than_n() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    for g in 0..3 {
        put_group(&engine, "t", g, ((g + 1) * 10) as i64);
    }
    create_ordered_ghost(&engine, "t", "g_t", true);
    let served = top(&engine, "t", 100, "sum(amount)", "DESC");
    assert_eq!(served.len(), 3);
    assert_eq!(served, oracle(all(&engine, "t", "sum(amount)"), 100, true));
}

/// ASC order.
#[test]
fn order_ascending() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    for g in 0..8 {
        put_group(&engine, "t", g, ((g + 1) * 10) as i64);
    }
    create_ordered_ghost(&engine, "t", "g_t", false);
    let served = top(&engine, "t", 3, "sum(amount)", "ASC");
    assert_eq!(served, oracle(all(&engine, "t", "sum(amount)"), 3, false));
}

/// A TOP whose direction differs from the declared order must NOT use the O(N)
/// path (it is emitted DESC-only) — it falls back to the O(M) quickselect and is
/// still correct. Guards the `descending` half of the match gate.
#[test]
fn mismatched_direction_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    for g in 0..6 {
        put_group(&engine, "t", g, ((g + 1) * 10) as i64);
    }
    create_ordered_ghost(&engine, "t", "g_t", true); // ordered by sum(amount) DESC
    // Query ASC — direction mismatch → O(M) fallback, ascending oracle.
    let served = top(&engine, "t", 3, "sum(amount)", "ASC");
    assert_eq!(served, oracle(all(&engine, "t", "sum(amount)"), 3, false));
}
