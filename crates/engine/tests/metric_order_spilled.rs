//! Spilled-ghost teeth: when a ghost's groups spill to the rollup keyspace, the
//! metric-order is emitted from the finalized on-disk rollups (the `None` source
//! branch), not an in-RAM map — the exact path Q4's 136k-group ghost takes. TOP
//! must still be bit-identical to sort-all-then-truncate. A tiny group budget
//! forces the spill with a handful of groups. This file is its own test binary
//! with a single test, so the process-global spill-limit env is not shared.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

fn rows(qr: QueryResult) -> Vec<(String, f64)> {
    match qr {
        QueryResult::GroupedAggregation(v) => v
            .into_iter()
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
            .collect(),
        other => panic!("expected grouped aggregation, got {other:?}"),
    }
}

fn oracle(mut all: Vec<(String, f64)>, n: usize) -> Vec<(String, f64)> {
    all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    all.truncate(n);
    all
}

#[test]
fn spilled_order_topn_equals_oracle() {
    // Force the spill path with a tiny in-RAM group budget.
    // SAFETY: single-test binary — no other thread reads the env concurrently.
    unsafe {
        std::env::set_var("XYZ_GHOST_SUMMARIES_MAX_GROUPS", "4");
    }
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "s""#);
    for g in 0..12 {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type:"R", grp:"g{g:02}", amount:{}}} IN "s""#,
                (g + 1) * 10
            ),
        );
        exec(
            &engine,
            &format!(r#"PUT {{_type:"R", grp:"g{g:02}", amount:5}} IN "s""#),
        );
    }
    exec(
        &engine,
        r#"CREATE GHOST "g_s" FROM "s" ORDER BY sum(amount) DESC GROUP BY grp AGGREGATE sum(amount)"#,
    );

    // Confirm the ghost spilled AND the order was emitted (so the O(N) read is
    // actually exercised over the on-disk rollups, not vacuously falling back).
    let shown = match exec(&engine, "SHOW GHOSTS") {
        QueryResult::Info(v) => v.join("\n"),
        other => panic!("expected info, got {other:?}"),
    };
    assert!(
        shown.contains("metric-order sum(amount) DESC") && shown.contains("emitted"),
        "SHOW GHOSTS must report the emitted order:\n{shown}"
    );

    let all: Vec<(String, f64)> = rows(exec(
        &engine,
        r#"SCAN "s" | GROUP BY grp | AGGREGATE sum(amount)"#,
    ));
    for n in [1usize, 3, 12] {
        let served = rows(exec(
            &engine,
            &format!(
                r#"SCAN "s" | GROUP BY grp | AGGREGATE sum(amount) | TOP {n} BY sum(amount) DESC"#
            ),
        ));
        assert_eq!(
            served,
            oracle(all.clone(), n),
            "spilled O(N) != oracle (n={n})"
        );
    }
}
