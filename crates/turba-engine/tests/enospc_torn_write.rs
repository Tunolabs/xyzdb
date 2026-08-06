//! 3e — disk-full (ENOSPC) mid-write must fail cleanly: no false ack, no
//! corruption, no partial apply, and every previously-acked write survives.
//!
//! A real disk-full mid-write leaves a PARTIAL WAL record on disk and returns
//! an error. The `FORCE_WRITE_ENOSPC` hook reproduces exactly that — flush
//! half the encoded batch to disk, then fail with ENOSPC — so the test
//! exercises the real torn-tail path, not just an early error return. The
//! invariant: (1) `commit` returns `Err`; (2) the failed batch is not applied
//! to any memtable (the WAL write precedes the memtable inserts in `commit`);
//! (3) recovery discards the partial record (entry.rs Start-without-End /
//! bad-checksum) so it neither corrupts the log nor resurrects the write; and
//! (4) all earlier acked writes are intact.
//!
//! Run: cargo test -p turba-engine --features durability-test-hooks --test enospc_torn_write

#![cfg(feature = "durability-test-hooks")]

// SPDX-License-Identifier: BUSL-1.1
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::{FORCE_WRITE_ENOSPC, PersistMode};

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

#[test]
fn enospc_mid_write_fails_clean_and_recovers() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");

    const N: u32 = 20;
    {
        let engine = TurbaEngine::open(&db, sync_config()).unwrap();

        // N acked, durable writes.
        for i in 0..N {
            let mut b = engine.batch();
            b.put_spatial(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes());
            b.commit().expect("acked write must succeed");
        }

        // Disk-full on the very next WAL write.
        FORCE_WRITE_ENOSPC.store(true, Ordering::Relaxed);
        let mut b = engine.batch();
        b.put_spatial(b"k_doomed", b"v_doomed");
        let res = b.commit();
        assert!(
            res.is_err(),
            "commit must return Err on ENOSPC mid-write, got {res:?} (no false ack)"
        );
        // Hook is one-shot; confirm it disarmed.
        assert!(
            !FORCE_WRITE_ENOSPC.load(Ordering::Relaxed),
            "hook must be one-shot"
        );

        // SIGKILL: forget bypasses Drop (no graceful flush). Recovery must
        // replay the WAL and discard the partial torn record.
        engine._test_release_dir_lock();
        std::mem::forget(engine);
    }

    let engine = TurbaEngine::open(&db, sync_config()).unwrap();
    // Every acked write survived...
    for i in 0..N {
        let got = engine.spatial.get(format!("k{i:04}").as_bytes()).unwrap();
        assert_eq!(
            got.as_deref(),
            Some(format!("v{i}").as_bytes()),
            "acked write k{i:04} lost after ENOSPC + crash recovery"
        );
    }
    // ...and the failed write neither applied nor resurrected (partial record
    // discarded — no corruption, no partial apply).
    assert!(
        engine.spatial.get(b"k_doomed").unwrap().is_none(),
        "the ENOSPC-failed write must not be recovered (it was never acked)"
    );
}
