//! Visible truncation is a UNIVERSAL guarantee: a SCAN never truncates
//! silently, whichever route the router picks. A no-LIMIT SCAN capped at the
//! default (1000) used to return a bare `Records` plus a server-side
//! `tracing::warn` the client never receives — a capped result indistinguishable
//! from a complete one. Every capped route now surfaces `has_more: true` so the
//! caller can tell the difference; a result under the cap must NOT falsely
//! signal. Signal-only (`cursor: None`) on the gravity fast path and the
//! ghost-routed path, which have no resumable cursor yet.
//!
//! Routes covered: gravity-indexed here, ghost-routed (`read_topn`) here, and the
//! `scan_primary_full_expr` fallback leaf in the engine's unit tests (those
//! fallbacks — ghost evicted mid-scan / unexpected PreComputed — are not
//! deterministically triggerable end to end). The full-lobe path already emits a
//! resumable cursor and is exercised by the pagination tests.

use xyzdb_engine::engine::{Engine, QueryResult};

/// Seed `n` records into a single gravity bucket (`*conv:"c1"`) so a
/// `WHERE conv="c1"` SCAN takes the gravity-indexed fast path.
fn seed(engine: &Engine, n: usize) {
    engine.run(r#"LOBE "mem""#).unwrap();
    for i in 0..n {
        engine
            .run(&format!(
                r#"PUT {{*conv:"c1", id:"r{i}", body:"m{i}"}} IN "mem""#
            ))
            .unwrap();
    }
}

#[test]
fn gravity_scan_over_cap_signals_has_more() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    // One past SCAN_LIMIT_DEFAULT (1000): the bucket genuinely overflows the cap.
    seed(&engine, 1001);

    // No LIMIT → default cap; single Eq on the gravity field → gravity-indexed.
    match engine.run(r#"SCAN "mem" WHERE conv="c1""#).unwrap() {
        QueryResult::PaginatedRecords {
            records,
            has_more,
            cursor,
        } => {
            assert_eq!(
                records.len(),
                1000,
                "result must be capped at the default (1000)"
            );
            assert!(has_more, "a truncated gravity scan must signal has_more");
            assert_eq!(
                cursor, None,
                "gravity path is signal-only (no resumable cursor yet)"
            );
        }
        other => panic!("expected PaginatedRecords{{has_more}}, got {other:?}"),
    }
}

#[test]
fn gravity_scan_within_cap_does_not_signal() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    seed(&engine, 10);

    // A complete result must not be dressed up as truncated. Either shape is
    // acceptable as long as it does not claim more rows remain.
    match engine.run(r#"SCAN "mem" WHERE conv="c1""#).unwrap() {
        QueryResult::Records(records) => assert_eq!(records.len(), 10),
        QueryResult::PaginatedRecords {
            records, has_more, ..
        } => {
            assert_eq!(records.len(), 10);
            assert!(
                !has_more,
                "a complete bucket must not falsely signal has_more"
            );
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn ghost_routed_scan_over_cap_signals_has_more() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "g""#).unwrap();
    // 1001 records, all matching `tag="t"`; no gravity field, so a filter SCAN
    // takes the ghost / full-lobe route rather than the gravity fast path.
    for i in 0..1001 {
        engine
            .run(&format!(r#"PUT {{numero:{i}, x:{i}, tag:"t"}} IN "g""#))
            .unwrap();
    }
    // A covering ordered ghost so a plain filter SCAN routes through read_topn.
    engine
        .run(r#"CREATE GHOST "gc" FROM "g" ORDER BY x"#)
        .unwrap();

    // No LIMIT, no ORDER BY → the ghost-routed filter-scan path (read_topn).
    match engine.run(r#"SCAN "g" WHERE tag="t""#).unwrap() {
        QueryResult::PaginatedRecords {
            records,
            has_more,
            cursor,
        } => {
            assert_eq!(
                records.len(),
                1000,
                "result must be capped at the default (1000)"
            );
            assert!(
                has_more,
                "a truncated ghost-routed scan must signal has_more"
            );
            // The ghost path is signal-only; if the router instead served this
            // from the full-lobe path it would carry a resumable cursor — either
            // way the truncation is visible, which is the guarantee under test.
            assert!(cursor.is_none(), "ghost-routed truncation is signal-only");
        }
        other => panic!("expected PaginatedRecords{{has_more}}, got {other:?}"),
    }
}
