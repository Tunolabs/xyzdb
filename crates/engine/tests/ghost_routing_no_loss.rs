//! Phase 3 — "no loss" differential oracle for ghost routing.
//!
//! Thesis: a query served via a covering ghost must return EXACTLY what the
//! primary lobe scan returns — no row missing, extra, or wrong. Each test runs
//! the SAME data in two lobes — `p` (no ghost → always Primary) and `g` (with a
//! ghost → may auto-route) — and asserts identical results. Any divergence is a
//! correctness loss.
//!
//! These tests are the investigation AND the guarantee: they pin every routing
//! scenario (empty-filter ghost, filtered ghost, range, extra predicates,
//! projection, NEAREST, multi-type, post-delete/update) against the oracle.

// SPDX-License-Identifier: BUSL-1.1
use std::collections::BTreeSet;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn recs(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected result: {other:?}"),
    }
}

/// Sorted set of the `numero` id field — order-independent identity of a result.
fn id_set(qr: QueryResult) -> BTreeSet<i64> {
    recs(qr)
        .iter()
        .filter_map(|r| match r.fields.get("numero") {
            Some(xyzdb_core::value::Value::Int(i)) => Some(*i),
            _ => None,
        })
        .collect()
}

/// Ordered list of `numero` — for NEAREST, where rank matters.
fn id_list(qr: QueryResult) -> Vec<i64> {
    recs(qr)
        .iter()
        .filter_map(|r| match r.fields.get("numero") {
            Some(xyzdb_core::value::Value::Int(i)) => Some(*i),
            _ => None,
        })
        .collect()
}

/// Run `q_template` against both lobes (`{L}` placeholder) and assert the
/// id-sets match. `ghost_ddl` creates the ghost on lobe `g` only.
fn assert_no_loss(seed: impl Fn(&Engine, &str), ghost_ddl: &str, q_template: &str) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed(&engine, "p");
    seed(&engine, "g");
    exec(&engine, ghost_ddl); // ghost on "g" only

    let primary = id_set(exec(&engine, &q_template.replace("{L}", "p")));
    let routed = id_set(exec(&engine, &q_template.replace("{L}", "g")));
    assert_eq!(
        primary, routed,
        "ghost route diverged from primary\n  query: {q_template}\n  ghost: {ghost_ddl}\n  primary={primary:?} routed={routed:?}"
    );
}

// ── 1. Empty-filter ghost: Eq + range on the ordered field ──────────────

fn seed_basic(engine: &Engine, l: &str) {
    // numero=id, x in 0..30, grp = id%3, flag = id%2==0
    for i in 0..30 {
        let grp = i % 3;
        let flag = if i % 2 == 0 { "true" } else { "false" };
        exec(
            engine,
            &format!(r#"PUT {{_type:"R", numero:{i}, x:{i}, grp:"g{grp}", flag:{flag}}} IN "{l}""#),
        );
    }
}

#[test]
fn eq_empty_ghost_matches_primary() {
    assert_no_loss(
        seed_basic,
        r#"CREATE GHOST "ge" FROM "g" ORDER BY x"#,
        r#"SCAN "{L}" WHERE x=12"#,
    );
}

#[test]
fn range_empty_ghost_matches_primary() {
    assert_no_loss(
        seed_basic,
        r#"CREATE GHOST "ge" FROM "g" ORDER BY x"#,
        r#"SCAN "{L}" WHERE x>=10 AND x<20"#,
    );
}

// ── 2. Filtered ghost, query carries the ghost's filter (F_g ⊆ F_q) ─────

#[test]
fn filtered_ghost_query_superset_matches() {
    assert_no_loss(
        seed_basic,
        r#"CREATE GHOST "gf" FROM "g" WHERE grp="g0" ORDER BY x"#,
        r#"SCAN "{L}" WHERE grp="g0" AND x>=6 AND x<24"#,
    );
}

// ── 3. Extra predicate beyond the ghost's filter, NON-projection ghost ──
// The full record is available (point-read), so the extra filter must apply.

#[test]
fn extra_filter_nonprojection_matches() {
    assert_no_loss(
        seed_basic,
        r#"CREATE GHOST "gf" FROM "g" WHERE grp="g0" ORDER BY x"#,
        r#"SCAN "{L}" WHERE grp="g0" AND flag=true"#,
    );
}

// ── 4. Extra predicate beyond the ghost's filter, PROJECTION ghost ──────
// Projection embeds x only; the query also filters `flag`. The projected record
// lacks `flag`, so the route must still apply it (point-read fallback) — else
// it silently returns rows ignoring flag (loss).

#[test]
fn extra_filter_projection_matches() {
    assert_no_loss(
        seed_basic,
        r#"CREATE GHOST "gp" FROM "g" WHERE grp="g0" ORDER BY x EMBED x"#,
        r#"SCAN "{L}" WHERE grp="g0" AND flag=true"#,
    );
}

// ── 5. Multi-type lobe: ghost on _type must not leak other types ────────

fn seed_multitype(engine: &Engine, l: &str) {
    for i in 0..30 {
        let t = if i % 2 == 0 { "A" } else { "B" };
        exec(
            engine,
            &format!(r#"PUT {{_type:"{t}", numero:{i}, x:{i}}} IN "{l}""#),
        );
    }
}

#[test]
fn multitype_ghost_no_leak() {
    assert_no_loss(
        seed_multitype,
        r#"CREATE GHOST "gt" FROM "g" WHERE _type="A" ORDER BY x"#,
        r#"SCAN "{L}" WHERE _type="A" AND x>=4 AND x<20"#,
    );
}

// ── 6. Consistency after DELETE and UPDATE ──────────────────────────────

#[test]
fn after_delete_matches_primary() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed_basic(&engine, "p");
    seed_basic(&engine, "g");
    exec(&engine, r#"CREATE GHOST "ge" FROM "g" ORDER BY x"#);
    for v in [12, 13, 14] {
        exec(&engine, &format!(r#"DELETE "p" WHERE x={v}"#));
        exec(&engine, &format!(r#"DELETE "g" WHERE x={v}"#));
    }
    let primary = id_set(exec(&engine, r#"SCAN "p" WHERE x>=10 AND x<20"#));
    let routed = id_set(exec(&engine, r#"SCAN "g" WHERE x>=10 AND x<20"#));
    assert_eq!(primary, routed, "ghost stale after deletes");
}

#[test]
fn after_update_sort_field_matches_primary() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed_basic(&engine, "p");
    seed_basic(&engine, "g");
    exec(&engine, r#"CREATE GHOST "ge" FROM "g" ORDER BY x"#);
    // Move some records to x=99.
    exec(&engine, r#"SET "p" x = 99 WHERE x=5"#);
    exec(&engine, r#"SET "g" x = 99 WHERE x=5"#);
    assert_eq!(
        id_set(exec(&engine, r#"SCAN "p" WHERE x=99"#)),
        id_set(exec(&engine, r#"SCAN "g" WHERE x=99"#)),
        "ghost stale after sort-field update"
    );
}

// ── 7. NEAREST after a filter that matches a scalar ghost (add.39) ───────

fn seed_vec(engine: &Engine, l: &str) {
    // 12 records, grp = id%2; emb is a 2D vector spread around the circle.
    for i in 0..12 {
        let grp = i % 2;
        let a = (i as f64) * 0.5;
        exec(
            engine,
            &format!(
                r#"PUT {{_type:"V", numero:{i}, grp:"g{grp}", emb:[{:.4},{:.4}]}} IN "{l}""#,
                a.cos(),
                a.sin()
            ),
        );
    }
}

#[test]
fn nearest_after_filter_nonprojection_ghost() {
    // Scalar ghost on grp; NEAREST needs `emb`. Non-projection → point-read
    // yields the full record incl. emb, so NEAREST must match primary.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed_vec(&engine, "p");
    seed_vec(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "gn" FROM "g" WHERE grp="g0" ORDER BY numero"#,
    );
    let q = r#"SCAN "{L}" WHERE grp="g0" | NEAREST(emb, [1.0, 0.0], 3, cosine)"#;
    assert_eq!(
        id_list(exec(&engine, &q.replace("{L}", "p"))),
        id_list(exec(&engine, &q.replace("{L}", "g"))),
        "NEAREST via scalar ghost diverged from primary"
    );
}

#[test]
fn nearest_after_filter_projection_ghost_omitting_vector() {
    // Projection embeds numero only (NOT emb). If the SCAN routes here and uses
    // the projection, NEAREST sees records without `emb` → wrong/empty. Must
    // match primary.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "p""#);
    exec(&engine, r#"LOBE "g""#);
    seed_vec(&engine, "p");
    seed_vec(&engine, "g");
    exec(
        &engine,
        r#"CREATE GHOST "gn" FROM "g" WHERE grp="g0" ORDER BY numero EMBED numero"#,
    );
    let q = r#"SCAN "{L}" WHERE grp="g0" | NEAREST(emb, [1.0, 0.0], 3, cosine)"#;
    assert_eq!(
        id_list(exec(&engine, &q.replace("{L}", "p"))),
        id_list(exec(&engine, &q.replace("{L}", "g"))),
        "NEAREST via projection ghost (no emb) diverged from primary"
    );
}
