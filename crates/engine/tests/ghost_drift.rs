//! Ghost drift guardrail — the mandatory safety net for the ghost redesign.
//!
//! Invariant: a ghost stays EXACT after inserts/updates/deletes that CROSS its
//! predicate (a record moving into/out of the ghost's WHERE filter), for both
//! its covering membership and its grouped aggregates. Driven through the REAL
//! path (verbs PUT/SET/DELETE/SCAN, not primitives).
//!
//! Oracle: two identical lobes — `p` (no ghost → primary/runtime path) and `g`
//! (ghost). The same write stream is applied to both; the ghost-served result on
//! `g` must equal the primary result on `p` after every mutation. Any divergence
//! is drift.
//!
//! The pre-existing `ghost_routing_no_loss.rs` covers unfiltered ghosts (deletes
//! / sort-field updates); this file closes the gap the redesign must not regress:
//! FILTERED ghosts whose membership changes when a write crosses the predicate,
//! plus grouped-aggregate exactness under the same writes.

use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

/// Set of `numero` ids in a Records result.
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

/// Seed `l` with a mix of active/inactive credits across two groups.
fn seed(engine: &Engine, l: &str) {
    for i in 0..12i64 {
        let status = if i % 3 == 0 { "inactive" } else { "active" };
        let grp = i % 2;
        // amount in integer cents (money) so the current f64 sum is exact at this N.
        let amount = 100 * (i + 1);
        exec(
            engine,
            &format!(
                r#"PUT {{_type:"Credit", numero:{i}, status:"{status}", grp:"g{grp}", x:{i}, amount:{amount}}} IN "{l}""#
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
fn covering_membership_exact_across_predicate_crossing_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
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

    // Update OUT of the predicate: an active record becomes inactive → must leave.
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET active→inactive (leaves filter)");

    // Update INTO the predicate: an inactive record becomes active → must enter.
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters filter)");

    // Delete an active (in-filter) record → must drop.
    both(&engine, r#"DELETE "{L}" WHERE numero = 2"#);
    check(&engine, "DELETE active member");

    // Insert a new active record → must appear.
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"active", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new active member");

    // Insert a new inactive record → must NOT appear.
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:98, status:"inactive", grp:"g1", x:98, amount:9800} IN "{L}""#,
    );
    check(&engine, "PUT new inactive non-member");
}

#[test]
fn grouped_aggregate_exact_across_predicate_crossing_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    // Filtered grouped-aggregate ghost on `g` only.
    exec(
        &engine,
        r#"CREATE GHOST "ga" FROM "g" WHERE status = "active" ORDER BY grp GROUP BY grp AGGREGATE sum(amount), count()"#,
    );

    let q = r#"SCAN "{L}" WHERE status = "active" | GROUP BY grp | AGGREGATE sum(amount), count()"#;
    let check = |engine: &Engine, msg: &str| {
        let p = grouped_named(exec(engine, &q.replace("{L}", "p")));
        let g = grouped_named(exec(engine, &q.replace("{L}", "g")));
        assert_eq!(
            p, g,
            "ghost aggregate drifted after {msg}\n  primary={p:?}\n  ghost={g:?}"
        );
    };
    check(&engine, "build");

    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET active→inactive (leaves group sum)");

    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters group sum)");

    both(&engine, r#"DELETE "{L}" WHERE numero = 2"#);
    check(&engine, "DELETE active member (group sum drops)");

    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"active", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new active member (group sum adds)");

    // Move a record BETWEEN groups while staying in the filter → both groups shift.
    both(&engine, r#"SET "{L}" grp = "g1" WHERE numero = 4"#);
    check(&engine, "SET grp g0→g1 (regroup within filter)");
}

#[test]
fn global_aggregate_exact_across_predicate_crossing_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    // Filtered GLOBAL-aggregate ghost (no GROUP BY) on `g` only. Guards the
    // "not grouped" branch of the ghost read path — the branch the Grouping
    // migration turns from an implicit `group_fields.is_empty()` into an
    // explicit `Grouping::Global`.
    exec(
        &engine,
        r#"CREATE GHOST "gg" FROM "g" WHERE status = "active" ORDER BY x AGGREGATE sum(amount), count()"#,
    );

    let q = r#"SCAN "{L}" WHERE status = "active" | AGGREGATE sum(amount), count()"#;
    let check = |engine: &Engine, msg: &str| {
        let p = grouped_named(exec(engine, &q.replace("{L}", "p")));
        let g = grouped_named(exec(engine, &q.replace("{L}", "g")));
        assert_eq!(
            p, g,
            "ghost global aggregate drifted after {msg}\n  primary={p:?}\n  ghost={g:?}"
        );
    };
    check(&engine, "build");

    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET active→inactive (leaves global sum)");

    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters global sum)");

    both(&engine, r#"DELETE "{L}" WHERE numero = 2"#);
    check(&engine, "DELETE active member (global sum drops)");

    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"active", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new active member (global sum adds)");
}

/// Set of `numero` ids from an explicit `SCAN GHOST "name"` — the entry-index
/// read path (Q4). Same shape as `id_set` but named for intent.
fn ghost_member_ids(qr: QueryResult) -> std::collections::BTreeSet<i64> {
    id_set(qr)
}

#[test]
fn covering_scan_ghost_reads_all_members_across_predicate_crossing_writes() {
    // A covering ghost keys one entry per member record; `SCAN GHOST` reads them
    // back. Exact vs the primary filter scan, across writes that cross the
    // predicate. Guards the entry-index that the GhostContent overlay keeps
    // universal.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "cov" FROM "g" WHERE status = "active" ORDER BY x"#,
    );

    let check = |engine: &Engine, msg: &str| {
        let p = id_set(exec(
            engine,
            r#"SCAN "p" WHERE status = "active" LIMIT 1000"#,
        ));
        let g = ghost_member_ids(exec(engine, r#"SCAN GHOST "cov" LIMIT 1000"#));
        assert_eq!(
            p, g,
            "covering SCAN GHOST drifted after {msg}\n  primary={p:?} ghost={g:?}"
        );
    };
    check(&engine, "build");
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET active→inactive (member leaves)");
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (member enters)");
    both(&engine, r#"DELETE "{L}" WHERE numero = 2"#);
    check(&engine, "DELETE member");
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"active", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new member");
}

#[test]
fn aggregate_scan_ghost_still_reads_members_across_predicate_crossing_writes() {
    // DUALITY GUARD (Q4). A GLOBAL-aggregate ghost ALSO keeps a per-record entry
    // index: `SCAN GHOST` returns the member RECORDS, not the summaries. This is
    // the seam the GhostContent overlay must preserve — entries stay universal,
    // an aggregate is only an overlay on top. It fails the day an aggregate
    // ghost stops serving its members via SCAN GHOST.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "agg" FROM "g" WHERE status = "active" ORDER BY x AGGREGATE sum(amount), count()"#,
    );

    let check = |engine: &Engine, msg: &str| {
        let p = id_set(exec(
            engine,
            r#"SCAN "p" WHERE status = "active" LIMIT 1000"#,
        ));
        let g = ghost_member_ids(exec(engine, r#"SCAN GHOST "agg" LIMIT 1000"#));
        assert_eq!(
            p, g,
            "aggregate SCAN GHOST (member records) drifted after {msg}\n  primary={p:?} ghost={g:?}"
        );
    };
    check(&engine, "build");
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET active→inactive (member leaves)");
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (member enters)");
    both(&engine, r#"DELETE "{L}" WHERE numero = 2"#);
    check(&engine, "DELETE member");
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"active", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new member");
}

#[test]
fn covering_ghost_survives_regravitating_set() {
    // A SET on a no-anchor lobe re-gravitates the record: with no GRAVITY BY
    // and no anchor, the gravity hash falls back to hashing all fields, so ANY
    // field change moves the record's spatial key. A covering ghost whose
    // ORDER BY value is unchanged must still resolve the record — its entry has
    // to follow the record to the new spatial key, not dangle on the removed
    // old one. Regression for silent covering loss: `index_count` kept counting
    // a record whose entry no longer point-read.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "g""#);
    for i in 0..5i64 {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"Credit", numero:{i}, status:"active", x:{i}}} IN "g""#),
        );
    }
    exec(
        &engine,
        r#"CREATE GHOST "gc" FROM "g" WHERE _type = "Credit" ORDER BY x"#,
    );
    let before = id_set(exec(&engine, r#"SCAN GHOST "gc" LIMIT 100"#));
    assert_eq!(before.len(), 5, "all members present at build");

    // SET a NEUTRAL field — not the filter field (_type), not the sort field
    // (x). It re-gravitates the record (sort unchanged) and must NOT drop it
    // from the covering ghost.
    exec(&engine, r#"SET "g" note = "touched" WHERE numero = 2"#);
    let after = id_set(exec(&engine, r#"SCAN GHOST "gc" LIMIT 100"#));
    assert_eq!(
        after, before,
        "re-gravitating SET dropped a covering member (stale entry key)\n  before={before:?} after={after:?}"
    );
}
#[test]
fn in_filter_membership_exact_across_set_crossing_writes() {
    // `FilterOp::In` under the ghost delta path: an update that moves a record
    // in/out of the IN set must add/remove it, exact vs the primary IN scan.
    // Red→green — a `WHERE ... IN (...)` ghost did not even parse before 2.1.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    // Covering ghost filtered by an IN set on `g` only.
    exec(
        &engine,
        r#"CREATE GHOST "gi" FROM "g" WHERE status IN ("active", "pending") ORDER BY x"#,
    );

    let check = |engine: &Engine, msg: &str| {
        let p = id_set(exec(
            engine,
            r#"SCAN "p" WHERE status IN ("active", "pending") LIMIT 1000"#,
        ));
        let g = id_set(exec(engine, r#"SCAN GHOST "gi" LIMIT 1000"#));
        assert_eq!(
            p, g,
            "IN membership drifted after {msg}\n  primary={p:?} ghost={g:?}"
        );
    };
    check(&engine, "build");

    // active → pending: both in the set, stays a member.
    both(&engine, r#"SET "{L}" status = "pending" WHERE numero = 1"#);
    check(&engine, "SET active→pending (stays in set)");
    // pending → inactive: leaves the set.
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET pending→inactive (leaves set)");
    // inactive → active: enters the set.
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters set)");
    // New member (in the set) and non-member (outside it).
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"pending", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new pending member");
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:98, status:"inactive", grp:"g1", x:98, amount:9800} IN "{L}""#,
    );
    check(&engine, "PUT new inactive non-member");
}

#[test]
fn or_ghost_membership_exact_across_predicate_crossing_writes() {
    // CAPABILITY (2.3): a hand-created OR ghost — impossible before, since
    // CREATE GHOST parsed flat-AND only. Membership = (status active OR pending)
    // must stay exact vs the primary OR scan under writes that cross the OR.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "gor" FROM "g" WHERE status = "active" OR status = "pending" ORDER BY x"#,
    );

    let check = |engine: &Engine, msg: &str| {
        let p = id_set(exec(
            engine,
            r#"SCAN "p" WHERE status = "active" OR status = "pending" LIMIT 1000"#,
        ));
        let g = id_set(exec(engine, r#"SCAN GHOST "gor" LIMIT 1000"#));
        assert_eq!(
            p, g,
            "OR ghost membership drifted after {msg}\n  primary={p:?} ghost={g:?}"
        );
    };
    check(&engine, "build");
    both(&engine, r#"SET "{L}" status = "pending" WHERE numero = 1"#);
    check(&engine, "SET active→pending (stays in OR)");
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET pending→inactive (leaves OR)");
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters OR)");
}

#[test]
fn not_ghost_membership_exact_across_predicate_crossing_writes() {
    // CAPABILITY (2.3): a hand-created NOT ghost. Membership = NOT(status
    // inactive) must stay exact vs the primary NOT scan under crossing writes.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "gnot" FROM "g" WHERE NOT status = "inactive" ORDER BY x"#,
    );

    let check = |engine: &Engine, msg: &str| {
        let p = id_set(exec(
            engine,
            r#"SCAN "p" WHERE NOT status = "inactive" LIMIT 1000"#,
        ));
        let g = id_set(exec(engine, r#"SCAN GHOST "gnot" LIMIT 1000"#));
        assert_eq!(
            p, g,
            "NOT ghost membership drifted after {msg}\n  primary={p:?} ghost={g:?}"
        );
    };
    check(&engine, "build");
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 1"#);
    check(&engine, "SET active→inactive (leaves NOT)");
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters NOT)");
}

/// Grouped aggregation compared BY NAME: per group, `label=value` for every
/// column. Proves the right column carries the right value, not just that the
/// numbers are present. Ghost and runtime now share ONE canonical label scheme
/// (`sum(field)`, `count`, aliases), so this holds across both paths for every
/// metric — that shared naming is itself part of what these tests lock.
fn grouped_named(qr: QueryResult) -> Vec<String> {
    let rows = match qr {
        QueryResult::GroupedAggregation(v) => v,
        QueryResult::Aggregation(m) => vec![m],
        other => panic!("expected (grouped) aggregation, got {other:?}"),
    };
    let mut out: Vec<String> = rows
        .into_iter()
        .map(|m| {
            let grp = m.get("grp").map(|v| format!("{v:?}")).unwrap_or_default();
            let mut cols: Vec<String> = m
                .iter()
                .filter(|(k, _)| k.as_str() != "grp")
                .map(|(k, v)| format!("{k}={v:?}"))
                .collect();
            cols.sort();
            format!("grp={grp}|{}", cols.join(";"))
        })
        .collect();
    out.sort();
    out
}

/// A2 — runtime per-metric conditional aggregates compute the exact expected
/// numbers. Each metric folds only the records passing its own WHERE, under its
/// AS alias. This is the correctness floor the ghost precompute must match.
#[test]
fn per_metric_filters_runtime_exact_values() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    seed(&engine, "p");

    // count() = all in group; active = count of status="active"; active_amt =
    // sum(amount) over status="active". Seed: 6 rows/group, 4 active/group,
    // active amounts sum to 2800 in each group (see `seed`).
    let q = r#"SCAN "p" | GROUP BY grp | AGGREGATE count(), count() AS active WHERE status = "active", sum(amount) AS active_amt WHERE status = "active""#;
    let got = grouped_named(exec(&engine, q));
    assert_eq!(
        got,
        vec![
            "grp=Text(\"g0\")|active=Int(4);active_amt=Float(2800.0);count=Int(6)".to_string(),
            "grp=Text(\"g1\")|active=Int(4);active_amt=Float(2800.0);count=Int(6)".to_string(),
        ],
        "per-metric conditional aggregates wrong: {got:?}"
    );
}

/// A2 — a composite ghost with per-metric filters stays EXACT (by name) vs the
/// runtime path across writes that cross a per-metric predicate. The ghost has
/// no header WHERE (covers the whole lobe); the per-metric filters do the
/// gating, so a status flip moves a record between the conditional metrics while
/// the total `count` is unchanged — the seam this feature introduces.
#[test]
fn per_metric_ghost_matches_runtime_by_name_across_writes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "gq" FROM "g" ORDER BY x GROUP BY grp AGGREGATE count(), count() AS active WHERE status = "active", sum(amount) AS active_amt WHERE status = "active""#,
    );

    let q = r#"SCAN "{L}" | GROUP BY grp | AGGREGATE count(), count() AS active WHERE status = "active", sum(amount) AS active_amt WHERE status = "active""#;
    let check = |engine: &Engine, msg: &str| {
        let p = grouped_named(exec(engine, &q.replace("{L}", "p")));
        let g = grouped_named(exec(engine, &q.replace("{L}", "g")));
        assert_eq!(
            p, g,
            "per-metric ghost drifted after {msg}\n  primary={p:?}\n  ghost={g:?}"
        );
    };
    check(&engine, "build");

    // Cross a per-metric filter: active→inactive leaves `active`/`active_amt`
    // but keeps the total `count` (record still in the group).
    both(&engine, r#"SET "{L}" status = "inactive" WHERE numero = 2"#);
    check(&engine, "SET active→inactive (leaves conditional metrics)");

    // Cross the other way: inactive→active enters the conditional metrics.
    both(&engine, r#"SET "{L}" status = "active" WHERE numero = 0"#);
    check(&engine, "SET inactive→active (enters conditional metrics)");

    // Delete an active member: total and conditional metrics both drop.
    both(&engine, r#"DELETE "{L}" WHERE numero = 4"#);
    check(&engine, "DELETE active member");

    // New active member: total and conditional metrics both add.
    both(
        &engine,
        r#"PUT {_type:"Credit", numero:99, status:"active", grp:"g0", x:99, amount:9900} IN "{L}""#,
    );
    check(&engine, "PUT new active member");
}

/// Global (non-grouped) aggregation as a label→value map.
fn agg_map(qr: QueryResult) -> std::collections::BTreeMap<String, xyzdb_core::value::Value> {
    match qr {
        QueryResult::Aggregation(m) => m,
        other => panic!("expected aggregation, got {other:?}"),
    }
}

/// Router metric-match guard: a ghost that precomputes `sum(b)` must NOT serve a
/// query asking `sum(a)`. Without the guard the router would return the ghost's
/// `sum(b)` under the wrong name (the pre-existing coarseness A2 sharpened);
/// with it the query falls back to a correct primary scan.
#[test]
fn aggregate_query_not_served_by_ghost_lacking_the_metric() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    for i in 0..6i64 {
        both(
            &engine,
            &format!(r#"PUT {{numero:{i}, a:{i}, b:{}}} IN "{{L}}""#, i * 100),
        );
    }
    // Ghost on `g` precomputes sum(b) only — NOT sum(a).
    exec(
        &engine,
        r#"CREATE GHOST "gsb" FROM "g" ORDER BY numero AGGREGATE sum(b)"#,
    );

    let q = r#"SCAN "{L}" | AGGREGATE sum(a)"#;
    let p = agg_map(exec(&engine, &q.replace("{L}", "p")));
    let g = agg_map(exec(&engine, &q.replace("{L}", "g")));
    assert_eq!(
        p, g,
        "sum(a) on the ghost lobe must equal the plain-lobe primary result \
         (metric-match guard → primary fallback)\n  p={p:?}\n  g={g:?}"
    );
    // sum(a) = 0+1+2+3+4+5 = 15; the ghost's sum(b) (1500) must never appear.
    let has_15 = g
        .values()
        .any(|v| matches!(v, xyzdb_core::value::Value::Float(f) if (*f - 15.0).abs() < 1e-9));
    let has_1500 = g
        .values()
        .any(|v| matches!(v, xyzdb_core::value::Value::Float(f) if (*f - 1500.0).abs() < 1e-9));
    assert!(has_15, "correct sum(a)=15 missing: {g:?}");
    assert!(
        !has_1500,
        "ghost's sum(b)=1500 leaked into a sum(a) query: {g:?}"
    );
}

/// Router metric-match guard, per-metric sibling (opened by A2): a ghost whose
/// conditional metric filters on status="active" must NOT serve a query whose
/// same-op metric filters on status="paid" — different filter, different metric.
#[test]
fn per_metric_query_not_served_by_ghost_with_different_filter() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    // Ghost precomputes count() WHERE status="active" (grouped by grp).
    exec(
        &engine,
        r#"CREATE GHOST "gpm" FROM "g" ORDER BY grp GROUP BY grp AGGREGATE count() AS active WHERE status = "active""#,
    );

    // Query asks for a DIFFERENT conditional count (status="paid").
    let q = r#"SCAN "{L}" | GROUP BY grp | AGGREGATE count() AS paid WHERE status = "paid""#;
    let p = grouped_named(exec(&engine, &q.replace("{L}", "p")));
    let g = grouped_named(exec(&engine, &q.replace("{L}", "g")));
    assert_eq!(
        p, g,
        "a paid-count query must not be served the ghost's active-count \
         (metric-match guard → primary fallback)\n  p={p:?}\n  g={g:?}"
    );
}

/// `SHOW GHOSTS` output as one string.
fn show_ghosts(engine: &Engine) -> String {
    match exec(engine, "SHOW GHOSTS") {
        QueryResult::Info(lines) => lines.join("\n"),
        other => panic!("expected info, got {other:?}"),
    }
}

/// A3 — Min/Max option D + observable staleness. Min/Max are exact on build and
/// refresh; a delete can't be decremented incrementally, so it marks the
/// aggregates stale — and that staleness is VISIBLE in `SHOW GHOSTS`, not just a
/// tracing warning. `REFRESH GHOST` rebuilds from source and clears it.
#[test]
fn minmax_option_d_stale_is_visible_and_refresh_reconciles() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "g""#);
    for i in 1..=5i64 {
        exec(
            &engine,
            &format!(r#"PUT {{numero:{i}, amount:{}}} IN "g""#, i * 100),
        );
    }
    exec(
        &engine,
        r#"CREATE GHOST "gmm" FROM "g" ORDER BY numero AGGREGATE min(amount), max(amount)"#,
    );

    let q = r#"SCAN "g" | AGGREGATE min(amount), max(amount)"#;
    // Min/Max preserve the source value's type on the ghost path (Int here),
    // unlike sum/avg which are always Float — accept either as a number.
    let f = |m: &std::collections::BTreeMap<String, xyzdb_core::value::Value>, k: &str| -> f64 {
        match m.get(k) {
            Some(xyzdb_core::value::Value::Float(v)) => *v,
            Some(xyzdb_core::value::Value::Int(v)) => *v as f64,
            other => panic!("missing {k}: {other:?}"),
        }
    };

    // Exact on build.
    let before = agg_map(exec(&engine, q));
    assert_eq!(f(&before, "min(amount)"), 100.0);
    assert_eq!(f(&before, "max(amount)"), 500.0);
    assert!(
        !show_ghosts(&engine).contains("aggregates stale"),
        "fresh ghost must not report stale aggregates"
    );

    // Deleting the min member can't be decremented from Min/Max → stale, visibly.
    exec(&engine, r#"DELETE "g" WHERE numero = 1"#);
    assert!(
        show_ghosts(&engine).contains("aggregates stale"),
        "a min/max delete must make the ghost report stale aggregates:\n{}",
        show_ghosts(&engine)
    );

    // REFRESH rebuilds from source → exact again, and the flag clears.
    exec(&engine, r#"REFRESH GHOST "gmm""#);
    assert!(
        !show_ghosts(&engine).contains("aggregates stale"),
        "REFRESH must clear the stale flag:\n{}",
        show_ghosts(&engine)
    );
    let after = agg_map(exec(&engine, q));
    assert_eq!(
        f(&after, "min(amount)"),
        200.0,
        "min must reconcile to 200 after refresh"
    );
    assert_eq!(f(&after, "max(amount)"), 500.0);
}
