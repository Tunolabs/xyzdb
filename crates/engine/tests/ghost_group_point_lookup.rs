//! Q2 fix: serving a fully-pinned group from an aggregate ghost must be a
//! point lookup on `group_summaries`, not an O(N) scan over every group.
//!
//! `read_aggregates` builds the exact group key (fragments joined by '|',
//! matching `extract_group_key`) and does `group_summaries.get(key)` when every
//! group field carries an Eq predicate. This test pins the CORRECTNESS of that
//! key construction — the risk being that a mis-encoded key misses a group the
//! old linear filter would have found. It exercises single-field, multi-field,
//! wildcard, and missing-group cases against a ghost with many groups.

use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    let stmt = xytalk_parser::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e:?}"));
    engine
        .execute(stmt)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn rows(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        QueryResult::GroupedAggregation(g) => g.len(),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn single_group_lookup_selects_the_right_group_among_many() {
    const GROUPS: usize = 1000;
    const PER_GROUP: usize = 3;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);
    // Aggregate ghost grouped by rfc — the Q2 shape (credits_by_rfc).
    exec(
        &engine,
        r#"CREATE GHOST "credits_by_rfc" FROM "creditos" WHERE _type = "Credit" ORDER BY rfc GROUP BY rfc AGGREGATE sum(monto), count()"#,
    );
    // Normal PUTs (no BULKMODE) → notify_write folds the aggregate ghost
    // incrementally, so group_summaries is populated and the aggregate query
    // routes to read_aggregates (the path under test).
    for g in 0..GROUPS {
        for k in 0..PER_GROUP {
            exec(
                &engine,
                &format!(
                    r#"PUT {{_type: "Credit", rfc: "RFC{g:05}", monto: {monto}}} IN "creditos""#,
                    monto = 100.0 + (k as f64)
                ),
            );
        }
    }

    // Single fully-pinned group → exactly one row (point lookup must find it
    // among the 1000 groups).
    let n = rows(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "RFC00500" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        n, 1,
        "single-rfc aggregate must return exactly its group, got {n}"
    );

    // A different pinned group → one row (not a stale/first group).
    let n2 = rows(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "RFC00001" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        n2, 1,
        "another single-rfc aggregate must return its group, got {n2}"
    );

    // Missing group → zero rows (point lookup must not invent a match).
    let n3 = rows(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" AND rfc = "RFC99999" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(n3, 0, "non-existent rfc must return no group, got {n3}");

    // Wildcard (no group filter) → every group (the non-point-lookup path
    // still works).
    let n4 = rows(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" | GROUP BY rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        n4, GROUPS,
        "wildcard aggregate must return all groups, got {n4}"
    );
}

#[test]
fn multi_field_group_point_lookup_builds_the_joined_key() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);
    // GROUP BY empresa_id, rfc — the multi-field shape (top_exposure). Key is
    // the two fragments joined by '|'; the point lookup must reconstruct it.
    exec(
        &engine,
        r#"CREATE GHOST "exp" FROM "creditos" WHERE _type = "Credit" ORDER BY empresa_id GROUP BY empresa_id, rfc AGGREGATE sum(monto), count()"#,
    );
    for emp in 0..20 {
        for r in 0..20 {
            exec(
                &engine,
                &format!(
                    r#"PUT {{_type: "Credit", empresa_id: "E{emp:03}", rfc: "R{r:03}", monto: 10.0}} IN "creditos""#
                ),
            );
        }
    }
    // Both group fields pinned → one exact group via the joined-key lookup.
    let n = rows(exec(
        &engine,
        r#"SCAN "creditos" WHERE _type = "Credit" AND empresa_id = "E007" AND rfc = "R013" | GROUP BY empresa_id, rfc | AGGREGATE sum(monto), count()"#,
    ));
    assert_eq!(
        n, 1,
        "fully-pinned multi-field group must return exactly one row, got {n}"
    );
}
