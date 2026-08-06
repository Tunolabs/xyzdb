//! Teeth for the `TOP n BY metric` pipeline step (server-side top-N over a
//! grouped aggregate). Attacks the edges, not the happy path:
//!   - oracle equivalence: server top-N == sort-all-groups + truncate;
//!   - M < N: fewer groups than the limit returns all of them;
//!   - ties in the metric at the N/N+1 cut: broken deterministically by group
//!     key (ascending), so the survivor set and order are stable;
//!   - ghost == runtime: the TOP result is identical whether the group
//!     aggregate came from a ghost (PreComputed) or a runtime scan.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

/// Ordered (group, sum) pairs from a GroupedAggregation result.
fn rows(qr: QueryResult) -> Vec<(String, f64)> {
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

/// Oracle: take ALL groups, apply the same total order (sum DESC, group ASC),
/// truncate to n. This is what the server-side TOP must reproduce exactly.
fn oracle(all: Vec<(String, f64)>, n: usize) -> Vec<(String, f64)> {
    let mut v = all;
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

/// Put one record per group `gNN` with `amount == amt` (so sum(amount) == amt).
fn put_group(engine: &Engine, lobe: &str, g: usize, amt: i64) {
    exec(
        engine,
        &format!(r#"PUT {{_type:"R", grp:"g{g:02}", amount:{amt}}} IN "{lobe}""#),
    );
}

const Q_TOP: &str = r#"SCAN "{L}" | GROUP BY grp | AGGREGATE sum(amount) | TOP {N} BY sum(amount)"#;
const Q_ALL: &str = r#"SCAN "{L}" | GROUP BY grp | AGGREGATE sum(amount)"#;

fn top(engine: &Engine, lobe: &str, n: usize) -> Vec<(String, f64)> {
    rows(exec(
        engine,
        &Q_TOP.replace("{L}", lobe).replace("{N}", &n.to_string()),
    ))
}
fn all(engine: &Engine, lobe: &str) -> Vec<(String, f64)> {
    rows(exec(engine, &Q_ALL.replace("{L}", lobe)))
}

#[test]
fn top_n_equals_sort_all_then_truncate() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    // 10 groups, distinct sums 10..100.
    for g in 0..10 {
        put_group(&engine, "t", g, ((g + 1) * 10) as i64);
    }
    for n in [1usize, 3, 7] {
        let server = top(&engine, "t", n);
        let want = oracle(all(&engine, "t"), n);
        assert_eq!(server, want, "TOP {n} != sort-all+truncate");
        assert_eq!(server.len(), n, "TOP {n} should return exactly {n}");
    }
}

#[test]
fn top_n_fewer_groups_than_limit_returns_all() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    for g in 0..3 {
        put_group(&engine, "t", g, ((g + 1) * 10) as i64);
    }
    let server = top(&engine, "t", 100);
    let want = oracle(all(&engine, "t"), 100);
    assert_eq!(server.len(), 3, "M<N must return all M groups");
    assert_eq!(server, want);
}

#[test]
fn top_n_ties_at_cut_broken_by_group_key() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "t""#);
    // g00=100, then g01,g02,g03 all tie at 50, g04=10. TOP 2 must keep g00 then
    // g01 (smallest key among the 50-ties) — g02/g03 dropped at the N/N+1 cut.
    put_group(&engine, "t", 0, 100);
    put_group(&engine, "t", 1, 50);
    put_group(&engine, "t", 2, 50);
    put_group(&engine, "t", 3, 50);
    put_group(&engine, "t", 4, 10);
    let server = top(&engine, "t", 2);
    assert_eq!(
        server,
        vec![("g00".to_string(), 100.0), ("g01".to_string(), 50.0)],
        "tie at the cut must be broken by group key ascending (deterministic)"
    );
    assert_eq!(server, oracle(all(&engine, "t"), 2));
    // Determinism: repeated runs give the identical order.
    assert_eq!(server, top(&engine, "t", 2));
}

#[test]
fn top_n_ghost_equals_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#); // runtime (no ghost)
    exec(&engine, r#"LOBE "g""#); // ghost-backed
    for g in 0..12 {
        // Two records per group so the ghost rollup actually aggregates.
        put_group(&engine, "p", g, ((g + 1) * 10) as i64);
        put_group(&engine, "p", g, 5);
        put_group(&engine, "g", g, ((g + 1) * 10) as i64);
        put_group(&engine, "g", g, 5);
    }
    exec(
        &engine,
        r#"CREATE GHOST "gtop" FROM "g" ORDER BY grp GROUP BY grp AGGREGATE sum(amount)"#,
    );
    let runtime = top(&engine, "p", 5);
    let ghost = top(&engine, "g", 5);
    assert_eq!(runtime, ghost, "TOP over ghost != TOP over runtime");
    assert_eq!(runtime, oracle(all(&engine, "p"), 5));
}
