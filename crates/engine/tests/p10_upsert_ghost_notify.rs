//! P10 — `PUT ... ON CONFLICT UPDATE` (upsert) must notify ghosts. `ghost_drift`
//! proves a covering ghost stays exact under SET/DELETE/PUT-insert; it does NOT
//! test the upsert path, which is exactly the P10 gap: before the fix, the ON
//! CONFLICT UPDATE branch skipped `ghost_manager.notify_write`
//! (`crates/engine/src/ops/put.rs`), so an upsert that crosses a ghost's
//! predicate left the ghost stale until `REFRESH`.
//!
//! Oracle (same as ghost_drift): two identical lobes — `p` (no ghost) and `g`
//! (ghost). The same upsert stream hits both; the ghost-served result on `g`
//! must equal the primary result on `p`. This gate fails pre-P10 (drift) and
//! passes after.

use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

fn id_set(qr: QueryResult) -> std::collections::BTreeSet<i64> {
    let recs = match qr {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected records, got {other:?}"),
    };
    recs.into_iter()
        .filter_map(|r| match r.fields.get("numero") {
            Some(xyzdb_core::value::Value::Int(n)) => Some(*n),
            _ => None,
        })
        .collect()
}

/// Declare a unique anchor on `numero` (so ON CONFLICT UPDATE conflicts on it)
/// then seed a mix of active/inactive credits.
fn setup(engine: &Engine, l: &str) {
    exec(engine, &format!(r#"LOBE "{l}""#));
    exec(engine, &format!(r#"ANCHOR "numero" UNIQUE IN "{l}""#));
    for i in 0..12i64 {
        let status = if i % 3 == 0 { "inactive" } else { "active" };
        exec(
            engine,
            &format!(
                r#"PUT {{_type:"Credit", numero:{i}, status:"{status}", grp:"g{}", x:{i}, amount:{}}} IN "{l}""#,
                i % 2,
                100 * (i + 1)
            ),
        );
    }
}

/// Apply the same mutation to both lobes.
fn both(engine: &Engine, stmt_template: &str) {
    exec(engine, &stmt_template.replace("{L}", "p"));
    exec(engine, &stmt_template.replace("{L}", "g"));
}

#[test]
fn upsert_keeps_covering_ghost_exact() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    setup(&engine, "p");
    setup(&engine, "g");
    // Filtered covering ghost on `g` only.
    exec(
        &engine,
        r#"CREATE GHOST "gm" FROM "g" WHERE status = "active" ORDER BY x"#,
    );

    let q = r#"SCAN "{L}" WHERE status = "active" LIMIT 1000"#;
    let check = |engine: &Engine, msg: &str| {
        let p = id_set(exec(engine, &q.replace("{L}", "p")));
        let g = id_set(exec(engine, &q.replace("{L}", "g")));
        assert_eq!(
            p, g,
            "ghost membership drifted after {msg}\n  primary={p:?} ghost={g:?}"
        );
    };
    check(&engine, "build");

    // UPSERT that LEAVES the filter: numero 1 (active) → inactive, via ON
    // CONFLICT UPDATE. Primary drops it from the active set; the ghost must too.
    // Pre-P10 (no notify_write on the upsert path) the ghost keeps numero 1.
    both(
        &engine,
        r#"PUT {numero:1, status:"inactive", grp:"g1", x:1, amount:200} IN "{L}" ON CONFLICT UPDATE"#,
    );
    check(&engine, "upsert active→inactive (leaves filter)");

    // UPSERT that ENTERS the filter: numero 0 (inactive) → active.
    both(
        &engine,
        r#"PUT {numero:0, status:"active", grp:"g0", x:0, amount:100} IN "{L}" ON CONFLICT UPDATE"#,
    );
    check(&engine, "upsert inactive→active (enters filter)");
}
