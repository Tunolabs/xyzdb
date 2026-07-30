//! 5a/5b — the periodic/Batched persist() fsync path must poison + surface on
//! EIO, exactly like the group-commit path (3a). Before the fix, the Batched
//! flush timer's persist() swallowed the fsync error (`let _ = ...persist()`)
//! and kept acking, so a disk EIO silently left acked-but-buffered writes
//! non-durable = false durability (S1) on the one path 3a did not cover.
//!
//! Run: cargo test -p turba-engine --features durability-test-hooks --test batched_persist_poison

#![cfg(feature = "durability-test-hooks")]

use std::sync::atomic::Ordering;
use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::{FORCE_SYNC_DATA_ERROR, PersistMode};

fn buffer_config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::Buffer,
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
fn persist_fsync_error_poisons_and_surfaces() {
    let dir = TempDir::new().unwrap();
    let engine = TurbaEngine::open(&dir.path().join("db"), buffer_config()).unwrap();

    // A Buffer-mode write: acked, buffered, not yet fsynced.
    {
        let mut b = engine.batch();
        b.put_spatial(b"k1", b"v1");
        b.commit().unwrap();
    }

    // The periodic/Batched persist hits a disk EIO on fsync.
    FORCE_SYNC_DATA_ERROR.store(true, Ordering::Relaxed);
    let r = engine.persist();
    assert!(
        r.is_err(),
        "persist() must SURFACE the fsync EIO (not swallow it), got {r:?}"
    );

    // 3a parity: the WAL is now poisoned, so every subsequent commit FAILS
    // FAST instead of false-acking against an un-syncable journal. (Pre-fix,
    // persist swallowed the error and the WAL was never poisoned, so this
    // commit would have succeeded — a false ack.)
    let mut b = engine.batch();
    b.put_spatial(b"k2", b"v2");
    let r2 = b.commit();
    assert!(
        r2.is_err(),
        "commit after a persist() fsync failure must fail fast (no false ack), got {r2:?}"
    );

    FORCE_SYNC_DATA_ERROR.store(false, Ordering::Relaxed);
}
