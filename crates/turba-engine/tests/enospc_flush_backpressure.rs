//! 8b/8c — a disk-full that STALLS background flush/compaction must
//! back-pressure cleanly, never wedge. This is the angle the WAL-write
//! ENOSPC test (3e) did not cover: ENOSPC on the background SST flush, not on
//! the foreground commit. The invariant: while flush is jammed,
//!   - the flush surfaces the error (no silent swallow);
//!   - the sealed-memtable backlog persists (back-pressure has something to
//!     act on);
//!   - the engine stays RESPONSIVE — new writes still commit (the WAL path is
//!     independent) and reads still work (no deadlock / wedge);
//!   - `wait_compaction_settle` RETURNS (bounded loop) instead of hanging
//!     forever waiting for a drain that can never happen;
//! and once the disk is back, the backlog drains and the engine settles.
//!
//! Run: cargo test -p turba-engine --features durability-test-hooks --test enospc_flush_backpressure

#![cfg(feature = "durability-test-hooks")]

use std::sync::atomic::Ordering;
use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::flush::FORCE_FLUSH_ENOSPC;
use turba_engine::journal::writer::PersistMode;

fn sync_config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::SyncData,
        wal_path: None,
        wal_segment_max_bytes: 64 * 1024 * 1024,
        worker_threads: 1,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

#[test]
fn flush_enospc_backpressures_without_wedge_then_recovers() {
    let dir = TempDir::new().unwrap();
    let engine = TurbaEngine::open(&dir.path().join("db"), sync_config()).unwrap();

    for i in 0..50u32 {
        let mut b = engine.batch();
        b.put_spatial(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes());
        b.commit().unwrap();
    }

    // Disk-full jams background flush; seal so there is a sealed memtable that
    // cannot be flushed.
    FORCE_FLUSH_ENOSPC.store(true, Ordering::Relaxed);
    engine.spatial.seal_active();

    // The stalled flush SURFACES the error (no silent swallow) and the backlog
    // persists (so back-pressure has something to act on).
    let r = engine.spatial.flush_sealed();
    assert!(r.is_err(), "stalled flush must surface ENOSPC, got {r:?}");
    assert!(
        engine.spatial.sealed_memtable_count() > 0,
        "sealed backlog must persist while flush is jammed"
    );

    // NO WEDGE: under a jammed flush the engine is still responsive — a new
    // write commits (the WAL path is independent of flush) and reads work.
    {
        let mut b = engine.batch();
        b.put_spatial(b"live", b"yes");
        b.commit()
            .expect("writes must still commit while flush is jammed (no deadlock)");
    }
    assert_eq!(
        engine.spatial.get(b"k0000").unwrap().as_deref(),
        Some(b"v0".as_ref())
    );
    assert_eq!(
        engine.spatial.get(b"live").unwrap().as_deref(),
        Some(b"yes".as_ref())
    );

    // settle does NOT hang: it is a bounded loop, so it returns even though the
    // backlog can never drain while jammed. Reaching the next line is the proof
    // — an unbounded wait would block the test forever.
    engine.wait_compaction_settle();
    assert!(
        engine.spatial.sealed_memtable_count() > 0,
        "still jammed right after a bounded settle (sanity: settle returned without draining)"
    );

    // RECOVER: clear the disk-full; the backlog drains and the engine settles.
    FORCE_FLUSH_ENOSPC.store(false, Ordering::Relaxed);
    engine.wait_compaction_settle();
    assert_eq!(
        engine.spatial.sealed_memtable_count(),
        0,
        "sealed backlog must fully drain once the disk is back"
    );
    // Data intact across the whole episode.
    for i in 0..50u32 {
        assert_eq!(
            engine
                .spatial
                .get(format!("k{i:04}").as_bytes())
                .unwrap()
                .as_deref(),
            Some(format!("v{i}").as_bytes())
        );
    }
}
