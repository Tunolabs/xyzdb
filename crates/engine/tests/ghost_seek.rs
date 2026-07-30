//! Ghost-seek (Phase 1, Eq): `SCAN GHOST "g" WHERE ordered_field = X` narrows
//! the scan prefix to `[ghost_id]+encode_sort_key(X)` so prefix_iter seeks to
//! the matching block instead of scanning every entry.
//!
//! These tests pin CORRECTNESS against ground truth: the seek must return
//! EXACTLY the records the full prefix scan + filter would (no subset, no
//! neighbours), across the tricky cases — 1:N duplicates, prefix-related Text,
//! DESC ghosts, misses, compound filters, Int sort fields. The narrowing is an
//! I/O bound only; the post-scan filter still enforces the predicate.

use std::collections::BTreeSet;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected result: {other:?}"),
    }
}

/// Collect the `numero` field of every returned record into a sorted set, so a
/// test can assert the exact identity set the seek returned.
fn numeros(qr: QueryResult) -> BTreeSet<i64> {
    records(qr)
        .iter()
        .filter_map(|r| match r.fields.get("numero") {
            Some(xyzdb_core::value::Value::Int(i)) => Some(*i),
            _ => None,
        })
        .collect()
}

#[test]
fn eq_returns_all_matches_one_to_many() {
    // 5 records share hours=10, 5 share hours=20. WHERE hours=10 must return
    // exactly the first group — the seek prefix must capture every tiebreak,
    // not collapse to one.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);
    for i in 0..5 {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"T", numero:{i}, hours:10}} IN "tasks""#),
        );
    }
    for i in 5..10 {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"T", numero:{i}, hours:20}} IN "tasks""#),
        );
    }
    exec(&engine, r#"CREATE GHOST "g" FROM "tasks" ORDER BY hours"#);

    let got = numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours=10"#));
    assert_eq!(
        got,
        (0..5).collect(),
        "WHERE hours=10 must return all 5 hours=10 records"
    );
}

#[test]
fn eq_excludes_neighbours_and_prefix_text() {
    // Text sort values where "a" is a prefix of "ab"/"abc": WHERE code="a" must
    // return only the exact "a" records, never the prefix-extended ones.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);
    let codes = ["a", "ab", "a", "abc", "b"];
    for (i, c) in codes.iter().enumerate() {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"T", numero:{i}, code:"{c}"}} IN "tasks""#),
        );
    }
    exec(&engine, r#"CREATE GHOST "g" FROM "tasks" ORDER BY code"#);

    let got = numeros(exec(&engine, r#"SCAN GHOST "g" WHERE code="a""#));
    assert_eq!(
        got,
        BTreeSet::from([0, 2]),
        "only the exact \"a\" records (0,2), not ab/abc"
    );
}

#[test]
fn eq_on_desc_ghost() {
    // Inverted (DESC) ghost: the seek prefix is the negated encoding; it must
    // still match exactly value==X.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);
    for i in 0..9 {
        let h = (i % 3) * 10; // 0,10,20,0,10,20,...
        exec(
            &engine,
            &format!(r#"PUT {{_type:"T", numero:{i}, hours:{h}}} IN "tasks""#),
        );
    }
    exec(
        &engine,
        r#"CREATE GHOST "g" FROM "tasks" ORDER BY hours DESC"#,
    );

    let got = numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours=10"#));
    assert_eq!(
        got,
        BTreeSet::from([1, 4, 7]),
        "DESC ghost: WHERE hours=10 returns the hours=10 set"
    );
}

#[test]
fn eq_miss_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);
    for i in 0..5 {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"T", numero:{i}, hours:10}} IN "tasks""#),
        );
    }
    exec(&engine, r#"CREATE GHOST "g" FROM "tasks" ORDER BY hours"#);

    let got = numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours=999"#));
    assert!(got.is_empty(), "a value with no records returns nothing");
}

#[test]
fn eq_on_ordered_field_plus_extra_filter() {
    // WHERE hours=10 (ordered → seek) AND status="open" (extra → post-filter).
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "tasks""#);
    // numero 0,1 hours=10 open ; 2 hours=10 closed ; 3,4 hours=20 open
    let rows = [
        (0, 10, "open"),
        (1, 10, "open"),
        (2, 10, "closed"),
        (3, 20, "open"),
        (4, 20, "open"),
    ];
    for (n, h, s) in rows {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"T", numero:{n}, hours:{h}, status:"{s}"}} IN "tasks""#),
        );
    }
    exec(&engine, r#"CREATE GHOST "g" FROM "tasks" ORDER BY hours"#);

    let got = numeros(exec(
        &engine,
        r#"SCAN GHOST "g" WHERE hours=10 AND status="open""#,
    ));
    assert_eq!(
        got,
        BTreeSet::from([0, 1]),
        "seek narrows on hours, post-filter drops the closed one"
    );
}

#[test]
fn eq_matches_full_scan_for_text_id() {
    // doc_id-style: unique Text key, one match. Compare the seek result to the
    // unfiltered full-scan set (ground truth) for a sampling of ids.
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "k""#);
    for i in 0..200 {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"k", numero:{i}, doc_id:"d{i}"}} IN "k""#),
        );
    }
    exec(&engine, r#"CREATE GHOST "g" FROM "k" ORDER BY doc_id"#);

    for id in [0, 50, 123, 199] {
        let got = numeros(exec(
            &engine,
            &format!(r#"SCAN GHOST "g" WHERE doc_id="d{id}""#),
        ));
        assert_eq!(
            got,
            BTreeSet::from([id as i64]),
            "doc_id=d{id} returns exactly record {id}"
        );
    }
}

// ── Phase 2: range-seek (>, >=, <, <=, BETWEEN), ASC + DESC ──────────────

/// Build a lobe `r` with `numero == hours` for 0..n, plus a ghost on `hours`.
fn range_fixture(dir: &std::path::Path, descending: bool) -> Engine {
    let engine = Engine::open(dir).unwrap();
    exec(&engine, r#"LOBE "r""#);
    for i in 0..20 {
        exec(
            &engine,
            &format!(r#"PUT {{_type:"R", numero:{i}, hours:{i}}} IN "r""#),
        );
    }
    let ord = if descending {
        "ORDER BY hours DESC"
    } else {
        "ORDER BY hours"
    };
    exec(&engine, &format!(r#"CREATE GHOST "g" FROM "r" {ord}"#));
    engine
}

#[test]
fn range_gt_gte_lt_lte_asc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = range_fixture(dir.path(), false);

    assert_eq!(
        numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours>16"#)),
        BTreeSet::from([17, 18, 19]),
        "strict > excludes the bound"
    );
    assert_eq!(
        numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours>=16"#)),
        BTreeSet::from([16, 17, 18, 19]),
        ">= includes the bound"
    );
    assert_eq!(
        numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours<3"#)),
        BTreeSet::from([0, 1, 2]),
        "strict < excludes the bound"
    );
    assert_eq!(
        numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours<=3"#)),
        BTreeSet::from([0, 1, 2, 3]),
        "<= includes the bound"
    );
}

#[test]
fn range_between_asc_and_desc() {
    for desc in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let engine = range_fixture(dir.path(), desc);
        // BETWEEN is inclusive on both ends (desugars to >= AND <=).
        assert_eq!(
            numeros(exec(
                &engine,
                r#"SCAN GHOST "g" WHERE hours>=5 AND hours<=8"#
            )),
            BTreeSet::from([5, 6, 7, 8]),
            "closed range [5,8] (desc={desc})"
        );
        // Open-ended high, on a DESC ghost the byte window flips — must still be exact.
        assert_eq!(
            numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours>17"#)),
            BTreeSet::from([18, 19]),
            "open-high > on desc={desc}"
        );
        assert_eq!(
            numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours<2"#)),
            BTreeSet::from([0, 1]),
            "open-low < on desc={desc}"
        );
    }
}

#[test]
fn range_empty_and_full() {
    let dir = tempfile::tempdir().unwrap();
    let engine = range_fixture(dir.path(), false);
    assert!(
        numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours>100"#)).is_empty(),
        "range past the max is empty"
    );
    assert_eq!(
        numeros(exec(&engine, r#"SCAN GHOST "g" WHERE hours>=0"#)).len(),
        20,
        ">=0 covers everything"
    );
}
