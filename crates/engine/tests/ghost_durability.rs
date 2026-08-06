//! Ghost crash-staleness (1d, durability facet).
//!
//! A ghost is a covering index (materialized view) over a lobe. CREATE GHOST
//! builds it from a spatial scan; later writes update it incrementally via
//! `GhostLobeManager::notify_write -> ghost_insert_inner -> Tree::insert`, which
//! writes the ghost keyspace's ACTIVE MEMTABLE ONLY and bypasses the WAL
//! (ghost.rs, explicit comment). On reopen, `load_all` restores the ghost
//! METADATA from the dictionary and TRUSTS the persisted ghost keyspace — it
//! does NOT rebuild the index from the (WAL-durable) primary records (ghost.rs
//! "no keyspace rebuild"). So a crash that loses the unflushed ghost memtable
//! leaves the index STALE vs the primary records, yet it is served as
//! authoritative: `SCAN GHOST` silently returns fewer rows than exist.
//!
//! This repro TRIGGERS the fault (write -> crash -> reopen -> read).

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_engine::engine::{Engine, QueryResult};

fn run(engine: &Engine, s: &str) -> QueryResult {
    engine.run(s).unwrap_or_else(|e| panic!("{s:?}: {e:?}"))
}

fn count(r: QueryResult) -> usize {
    match r {
        QueryResult::Records(v) => v.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("expected Records, got {other:?}"),
    }
}

#[test]
fn ghost_index_consistent_with_records_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "items""#);

    // 5 matching records, then a ghost over them.
    for i in 0..5 {
        run(
            &engine,
            &format!(r#"PUT {{id: {i}, kind: "hot"}} IN "items""#),
        );
    }
    run(
        &engine,
        r#"CREATE GHOST "g_hot" FROM "items" WHERE kind = "hot" ORDER BY id"#,
    );

    // COMPACT flushes the ghost metadata + the CREATE-time index to SSTs so the
    // ghost durably survives the restart — this isolates STALENESS from total
    // ghost loss.
    run(&engine, "COMPACT");

    // 5 MORE matching records. These update the ghost incrementally (memtable
    // only, bypassing the WAL); the records themselves are WAL-durable.
    for i in 5..10 {
        run(
            &engine,
            &format!(r#"PUT {{id: {i}, kind: "hot"}} IN "items""#),
        );
    }
    assert_eq!(
        count(run(&engine, r#"SCAN GHOST "g_hot""#)),
        10,
        "pre-crash the ghost sees all 10 records"
    );

    // Crash before the ghost memtable flushes (SIGKILL).
    engine._test_release_dir_lock();
    std::mem::forget(engine);

    let engine = Engine::open(dir.path()).unwrap();
    // Unfiltered scan cannot be served by the `kind = "hot"` ghost, so it is the
    // true durable record count from the primary keyspace.
    let primary = count(run(&engine, r#"SCAN "items""#));
    let ghost = count(run(&engine, r#"SCAN GHOST "g_hot""#));
    // A plain filtered scan: the router silently routes it to the ghost.
    let routed = count(run(&engine, r#"SCAN "items" WHERE kind = "hot""#));
    eprintln!(
        "after crash: primary(unfiltered)={primary}  ghost={ghost}  routed_filtered={routed}"
    );

    assert_eq!(
        primary, 10,
        "all 10 records are WAL-durable and survive the crash"
    );
    assert_eq!(
        ghost, primary,
        "the ghost index must be consistent with the durable records after a \
         crash (got ghost={ghost}, primary={primary} — a stale ghost served as \
         authoritative returns silently wrong results)"
    );
    assert_eq!(
        routed, primary,
        "a plain filtered SCAN routed to the ghost must not return stale results \
         (got routed={routed}, primary={primary})"
    );
}

/// The clean-shutdown fast path: a clean Drop flushes the ghost memtable and
/// records the marker, so the next open trusts the persisted index without a
/// rebuild — and it is still consistent.
#[test]
fn ghost_index_survives_clean_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path()).unwrap();
        run(&engine, r#"LOBE "items""#);
        for i in 0..5 {
            run(
                &engine,
                &format!(r#"PUT {{id: {i}, kind: "hot"}} IN "items""#),
            );
        }
        run(
            &engine,
            r#"CREATE GHOST "g_hot" FROM "items" WHERE kind = "hot" ORDER BY id"#,
        );
        run(&engine, "COMPACT");
        for i in 5..10 {
            run(
                &engine,
                &format!(r#"PUT {{id: {i}, kind: "hot"}} IN "items""#),
            );
        }
        // clean shutdown: Drop flushes the ghost memtable + writes the marker.
    }

    let engine = Engine::open(dir.path()).unwrap();
    assert_eq!(
        count(run(&engine, r#"SCAN GHOST "g_hot""#)),
        10,
        "a clean shutdown flushes the ghost index; it must stay consistent"
    );
}
