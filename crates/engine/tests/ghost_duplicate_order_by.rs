//! Audit P0-2: ghost entry sort keys gained a spatial-key uniqueness suffix.
//!
//! Before the fix, the ghost entry key was `[ghost_id][type_tag][value_bytes]`
//! with no per-record disambiguator, so records sharing the same ORDER BY value
//! collapsed to one LSM key — the second insert shadowed the first — and a
//! covering ghost returned a subset (in `top_exposure` ORDER BY empresa_id, the
//! entries collapsed to ~1 per company). The fix appends the record's spatial
//! key to the sort key and encodes Text values prefix-free so the suffix never
//! perturbs ordering between distinct values.
//!
//! These tests pin that every record with a duplicate (or prefix-related Text)
//! sort value survives, on both the build path (CREATE GHOST after the PUTs)
//! and the incremental path (PUTs after CREATE GHOST).

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn count_records(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("unexpected result: {other:?}"),
    }
}

const N: usize = 20;

#[test]
fn build_path_keeps_all_records_sharing_a_sort_value() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);

    // N distinct records, all with the SAME ORDER BY value (hours = 10).
    for i in 0..N {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type: "Task", numero: {i}, hours: 10, status: "blocked"}} IN "tasks""#
            ),
        );
    }

    // Build path: the ghost is materialised from the existing records.
    exec(
        &engine,
        r#"CREATE GHOST "g" FROM "tasks" WHERE _type = "Task" AND status = "blocked" ORDER BY hours"#,
    );

    assert_eq!(
        count_records(exec(&engine, r#"SCAN GHOST "g""#)),
        N,
        "every record sharing the ORDER BY value must survive (build path)"
    );
}

#[test]
fn incremental_path_keeps_all_records_sharing_a_sort_value() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);

    // Ghost first → each PUT flows through notify_write's incremental insert.
    exec(
        &engine,
        r#"CREATE GHOST "g" FROM "tasks" WHERE _type = "Task" AND status = "blocked" ORDER BY hours"#,
    );
    for i in 0..N {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type: "Task", numero: {i}, hours: 10, status: "blocked"}} IN "tasks""#
            ),
        );
    }

    assert_eq!(
        count_records(exec(&engine, r#"SCAN GHOST "g""#)),
        N,
        "every record sharing the ORDER BY value must survive (incremental path)"
    );
}

#[test]
fn text_sort_values_that_share_a_prefix_do_not_alias() {
    // "a" and "ab" share a prefix; with the prefix-free Text encoding plus the
    // tiebreak, duplicates and prefix pairs all stay distinct in the ghost.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);

    let codes = ["a", "ab", "a", "ab", "abc"];
    for (i, code) in codes.iter().enumerate() {
        exec(
            &engine,
            &format!(
                r#"PUT {{_type: "Task", numero: {i}, code: "{code}", status: "blocked"}} IN "tasks""#
            ),
        );
    }

    exec(
        &engine,
        r#"CREATE GHOST "g" FROM "tasks" WHERE _type = "Task" AND status = "blocked" ORDER BY code"#,
    );

    assert_eq!(
        count_records(exec(&engine, r#"SCAN GHOST "g""#)),
        codes.len(),
        "duplicate and prefix-related Text sort values must all survive"
    );
}
