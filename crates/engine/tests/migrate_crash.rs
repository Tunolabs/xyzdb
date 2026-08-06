//! MIGRATE crash coverage (durability gate) — 0.9.4.
//!
//! MIGRATE (`engine/maintenance.rs`) rewrites a batch of gravity keys in windows
//! (`commit_migrate_window`, default ~10k). Its only safety net against an
//! interruption is idempotent re-run — which had NO crash test. This exercises
//! it end to end: force a crash mid-migration after one committed window, take a
//! real SIGKILL, reopen, and re-run. Every record must survive (no loss) and end
//! in the one gravity bucket with a distinct key (no aliasing). This is also the
//! durability precedent a future satellite re-pack would reuse — worth its own
//! coverage regardless of sub-gravity.
//!
//! The crash is injected via the test-only knobs `MIGRATE_WINDOW_LIMIT` (shrink
//! the window so a boundary is crossed without a 10k-row dataset) and
//! `FORCE_MIGRATE_ABORT_AFTER_WINDOWS` (abort after N committed windows).

// SPDX-License-Identifier: BUSL-1.1
use std::sync::atomic::Ordering;
use xyzdb_engine::engine::{
    Engine, FORCE_MIGRATE_ABORT_AFTER_WINDOWS, MIGRATE_WINDOW_LIMIT, QueryResult,
};

fn run(engine: &Engine, s: &str) -> QueryResult {
    engine.run(s).unwrap_or_else(|e| panic!("run {s:?}: {e:?}"))
}

/// Sorted `id` ints from a SCAN result.
fn scan_ids(engine: &Engine, q: &str) -> Vec<i64> {
    let qr = engine
        .run(q)
        .unwrap_or_else(|e| panic!("scan {q:?}: {e:?}"));
    let recs = match qr {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected Records, got {other:?}"),
    };
    let mut ids: Vec<i64> = recs
        .into_iter()
        .filter_map(|r| match r.fields.get("id") {
            Some(xyzdb_core::value::Value::Int(n)) => Some(*n),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn migrate_interrupted_mid_window_reruns_without_loss_or_aliasing() {
    const N: i64 = 5;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().to_path_buf();

    {
        let engine = Engine::open(&db).expect("open");
        run(&engine, r#"LOBE "m""#);
        // Write WITHOUT a gravity spec → each record lands at its own fallback
        // (fields-hash) bucket. Same `k` value across all, distinct `id`.
        for i in 0..N {
            run(&engine, &format!(r#"PUT {{id: {i}, k: "bucket"}} IN "m""#));
        }
        // Declare gravity AFTER the writes: every record's stored hash now
        // differs from the value-only hash of `k`, so MIGRATE must MOVE all N
        // into the single `k = "bucket"` gravity bucket.
        run(&engine, r#"GRAVITY BY k IN "m""#);

        // Crash mid-migration: tiny windows (2) + abort after the 1st committed
        // window → 2 records moved + durable, 3 still at their old keys.
        MIGRATE_WINDOW_LIMIT.store(2, Ordering::Relaxed);
        FORCE_MIGRATE_ABORT_AFTER_WINDOWS.store(1, Ordering::Relaxed);
        let err = engine
            .run("MIGRATE")
            .expect_err("MIGRATE must abort mid-window under the crash injection");
        assert!(
            format!("{err}").contains("interrupted"),
            "expected the crash-injection abort, got: {err}"
        );
        // Disarm so nothing else in the process is affected.
        MIGRATE_WINDOW_LIMIT.store(0, Ordering::Relaxed);
        FORCE_MIGRATE_ABORT_AFTER_WINDOWS.store(0, Ordering::Relaxed);

        // No loss even mid-migration: an unfiltered scan still sees all N
        // (2 at the new bucket, 3 at their old fallback buckets).
        assert_eq!(
            scan_ids(&engine, r#"SCAN "m" LIMIT 1000"#).len(),
            N as usize,
            "records lost by the aborted MIGRATE"
        );

        // Real SIGKILL: stop bg threads without flushing, then leak the engine.
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    // Reopen from disk (the committed window survived the crash) and re-run.
    let engine = Engine::open(&db).expect("reopen");
    run(&engine, "MIGRATE"); // idempotent: skips the 2 done, completes the 3 pending

    // No loss: every record present after crash + re-run.
    let all = scan_ids(&engine, r#"SCAN "m" LIMIT 1000"#);
    assert_eq!(
        all,
        (0..N).collect::<Vec<_>>(),
        "records lost across the MIGRATE crash + re-run"
    );
    // Convergence: the gravity-pinned scan finds every record in the one bucket.
    let pinned = scan_ids(&engine, r#"SCAN "m" WHERE k = "bucket" LIMIT 1000"#);
    assert_eq!(
        pinned,
        (0..N).collect::<Vec<_>>(),
        "MIGRATE did not converge all records into the gravity bucket"
    );
    // No aliasing: N records, N distinct ids — the `seq` tail keeps full keys
    // distinct even though they now share one gravity_hash.
    assert_eq!(pinned.len(), N as usize);
}
