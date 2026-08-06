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

// SPDX-License-Identifier: BUSL-1.1
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

/// Number of RECORDS returned — deliberately not `id_set().len()`, which is a
/// set of `numero` values and collapses exactly the duplicates these tests are
/// looking for. A duplicate-detection assertion built on a set cannot fail.
fn rec_count(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("expected records, got {other:?}"),
    }
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

/// The same gate through the BATCH door. `PUT BATCH … ON CONFLICT UPDATE` used
/// to insert a duplicate instead of updating; the fix routes the collision
/// through `execute_upsert`, the same function the single statement uses. That
/// is deliberate — merging inline in the batch loop would have skipped
/// `notify_write` and re-opened P10 on a second path — and this test is what
/// holds it there. Without it, nothing in the suite exercised a batch upsert at
/// all (the file above had no `PUT BATCH` in it).
#[test]
fn batch_upsert_keeps_covering_ghost_exact() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    setup(&engine, "p");
    setup(&engine, "g");
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

    // One batch carrying both directions at once: numero 2 leaves the filter,
    // numero 3 enters it. Both are collisions on the UNIQUE anchor `numero`.
    both(
        &engine,
        r#"PUT BATCH IN "{L}" [
             {numero:2, status:"inactive", grp:"g2", x:2, amount:300},
             {numero:3, status:"active",   grp:"g0", x:3, amount:400}
           ] ON CONFLICT UPDATE"#,
    );
    check(&engine, "batch upsert (one leaves, one enters)");

    // The anchor still holds: the upserts updated in place, they did not add a
    // second record under the same `numero`. This is the assertion that fails
    // against the pre-fix engine — the drift check above can pass while
    // duplicates accumulate, because both lobes duplicate identically.
    for numero in [2i64, 3] {
        let n = rec_count(exec(
            &engine,
            &format!(r#"SCAN "p" WHERE numero = {numero} LIMIT 1000"#),
        ));
        assert_eq!(
            n, 1,
            "numero {numero} must exist exactly once after a batch upsert, found {n}"
        );
    }
    // Total count is the real duplicate detector: 12 seeded, 0 added.
    assert_eq!(
        rec_count(exec(&engine, r#"SCAN "p" LIMIT 1000"#)),
        12,
        "batch upsert changed the record count; it must update in place"
    );
    // And the merge actually landed: numero 3 must now be active.
    assert!(
        id_set(exec(
            &engine,
            r#"SCAN "p" WHERE status = "active" LIMIT 1000"#
        ))
        .contains(&3),
        "the batch upsert did not apply its field merge"
    );
}
