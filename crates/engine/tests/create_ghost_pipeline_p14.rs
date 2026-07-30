//! Teeth for xyTalk v1 P14: `CREATE GHOST` accepts the canonical pipeline form
//! (`… | GROUP BY … | AGGREGATE … | TAKE BY <metric>`) as an alias of the
//! classic `ORDER BY … GROUP BY … AGGREGATE …` clause form. A ghost is a saved
//! query, so a pipeline-declared metric-order ghost must build and serve
//! `TAKE n BY <metric>` byte-identically to the clause-declared one.

use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

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

fn seed(engine: &Engine, lobe: &str) {
    exec(engine, &format!(r#"LOBE "{lobe}""#));
    for g in 0..12 {
        exec(
            engine,
            &format!(
                r#"PUT {{_type:"R", grp:"g{g:02}", amount:{}}} IN "{lobe}""#,
                (g + 1) * 10
            ),
        );
        exec(
            engine,
            &format!(r#"PUT {{_type:"R", grp:"g{g:02}", amount:5}} IN "{lobe}""#),
        );
    }
}

#[test]
fn pipeline_declared_ghost_serves_like_clause_declared() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine, "cl");
    seed(&engine, "pl");

    // Same ghost, two spellings — clause alias vs canonical pipeline.
    exec(
        &engine,
        r#"CREATE GHOST "gcl" FROM "cl" ORDER BY sum(amount) GROUP BY grp AGGREGATE sum(amount)"#,
    );
    exec(
        &engine,
        r#"CREATE GHOST "gpl" FROM "pl" | GROUP BY grp | AGGREGATE sum(amount) | TAKE BY sum(amount)"#,
    );

    let clause = grouped(exec(
        &engine,
        r#"SCAN "cl" | GROUP BY grp | AGGREGATE sum(amount) | TAKE 5 BY sum(amount)"#,
    ));
    let pipeline = grouped(exec(
        &engine,
        r#"SCAN "pl" | GROUP BY grp | AGGREGATE sum(amount) | TAKE 5 BY sum(amount)"#,
    ));
    assert_eq!(
        pipeline, clause,
        "a pipeline-declared ghost must serve TAKE identically to a clause-declared one"
    );
    assert_eq!(pipeline.len(), 5);
}
