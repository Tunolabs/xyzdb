//! fsyncgate (S1) regression for the WAL poison fix.
//!
//! A failed WAL `sync()` must POISON the WAL — never false-ack a write, never
//! retry (on Linux a retried fsync can return Ok after EIO without the bytes
//! ever reaching disk). And the durability contract must hold: every write
//! acked Ok BEFORE the failure survives a reopen.
//!
//! Run with the test hook:
//!   cargo test -p turba-engine --features durability-test-hooks --test fsync_poison
#![cfg(feature = "durability-test-hooks")]

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

fn sync_config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::SyncData,
        wal_path: None,
        wal_segment_max_bytes: 64 * 1024 * 1024,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

/// Case 1 — a commit whose fsync fails returns `Err` (never a false `Ok`), the
/// WAL is poisoned, and subsequent writes fail fast.
#[test]
fn fsync_error_commit_returns_err_and_poisons() {
    let dir = TempDir::new().unwrap();
    let engine = TurbaEngine::open(&dir.path().join("db"), sync_config()).unwrap();

    engine._test_force_sync_error(true);

    let mut b = engine.batch();
    b.put_spatial(b"k1", b"v1");
    let res = b.commit();
    assert!(
        res.is_err(),
        "commit must return Err when fsync fails, got {res:?}"
    );
    assert!(
        engine._test_is_poisoned(),
        "WAL must be poisoned after an fsync failure"
    );

    // A new write fails fast (does not block, does not false-ack).
    let mut b2 = engine.batch();
    b2.put_spatial(b"k2", b"v2");
    assert!(b2.commit().is_err(), "poisoned WAL must reject new writes");
}

/// Case 2 — many writers parked in the group-commit barrier when the poison
/// fires must ALL return `Err` and none may hang. This proves `notify_all`
/// under the lock wakes every waiter (no lost wakeup). A hang fails the test
/// via the harness timeout. The pause/unpause gives a controlled point where
/// N writers are concurrently waiting before the poison.
#[test]
fn concurrent_waiters_all_err_on_poison() {
    let dir = TempDir::new().unwrap();
    let engine = Arc::new(TurbaEngine::open(&dir.path().join("db"), sync_config()).unwrap());

    // Freeze the sync thread so every commit enqueues and parks in the wait
    // loop — a controlled point with N concurrent waiters.
    engine._test_pause_sync(true);

    let n = 8;
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let e = Arc::clone(&engine);
            std::thread::spawn(move || {
                let mut b = e.batch();
                b.put_spatial(format!("k{i}").as_bytes(), b"v");
                b.commit()
            })
        })
        .collect();

    // Let them reach the wait loop, then poison and release the sync thread.
    std::thread::sleep(Duration::from_millis(50));
    engine._test_force_sync_error(true);
    engine._test_pause_sync(false);

    for h in handles {
        let res = h.join().expect("waiter thread panicked");
        assert!(
            res.is_err(),
            "every concurrent waiter must get Err on poison, got {res:?}"
        );
    }
}

/// Case 3 (the contract) — every write acked `Ok` BEFORE the fsync failure must
/// survive a reopen. The in-process `Err` is necessary but not sufficient; this
/// is what "durable" means.
#[test]
fn acked_writes_before_fsync_error_survive_reopen() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");

    {
        let engine = TurbaEngine::open(&db, sync_config()).unwrap();
        // A and B are acked Ok → durable.
        let mut a = engine.batch();
        a.put_spatial(b"kA", b"vA");
        a.commit().expect("A must ack");
        let mut b = engine.batch();
        b.put_spatial(b"kB", b"vB");
        b.commit().expect("B must ack");

        // The next fsync fails; C must NOT be acked.
        engine._test_force_sync_error(true);
        let mut c = engine.batch();
        c.put_spatial(b"kC", b"vC");
        assert!(c.commit().is_err(), "C must not be acked once fsync fails");

        let _ = engine.shutdown();
    }

    // Reopen (fresh engine, poison cleared): recovery replays the durable WAL.
    let engine = TurbaEngine::open(&db, sync_config()).unwrap();
    assert_eq!(
        engine.spatial.get(b"kA").unwrap().as_deref(),
        Some(b"vA".as_ref()),
        "acked write A must survive the reopen"
    );
    assert_eq!(
        engine.spatial.get(b"kB").unwrap().as_deref(),
        Some(b"vB".as_ref()),
        "acked write B must survive the reopen"
    );
    // C's presence is unspecified: an injected EIO does not actually drop the
    // bytes, and a clean shutdown may flush them. The contract only guarantees
    // acked-before-failure writes (A, B).
}
