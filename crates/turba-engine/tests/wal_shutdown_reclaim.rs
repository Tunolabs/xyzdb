//! Regression: a graceful `shutdown()` must RECLAIM the WAL (deuda #10).
//!
//! Before the fix, `shutdown()` sealed + flushed every tree but left the WAL on
//! disk in full. The WAL-prune watermark is `min(manifest_durable_seqno)` across
//! all keyspaces, so a keyspace whose memtable never fills during a session pins
//! the watermark low and the background pruner cannot drop the (already-durable)
//! archived segments. The full WAL therefore survived a clean shutdown, which
//! (a) doubled the on-disk footprint (SSTables + a redundant WAL copy) and
//! (b) made the next `open()` replay the entire write history into one memtable
//! — OOM-killing a restart at a tight memory envelope (confirmed: 100k @256M
//! restart exited 137).
//!
//! The fix flushes every tree synchronously in `shutdown()` (advancing each
//! keyspace's manifest-durable seqno) and then rotates the WAL, so a clean
//! shutdown leaves only SSTables and recovery replays nothing. This test writes
//! enough to roll several archived WAL segments WITHOUT flushing — pinning the
//! watermark so the WAL is genuinely un-pruned at shutdown — then asserts the WAL
//! is empty after `shutdown()` and every record still recovers on reopen.

// SPDX-License-Identifier: BUSL-1.1
use std::path::Path;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

fn config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::SyncData,
        wal_path: None,
        // Tiny segments so several archived `journal.<n>.wal` files roll during
        // the load (the un-pruned state the fix must reclaim). Well below the
        // memtable flush threshold, so the memtable never auto-flushes and the
        // prune watermark stays pinned.
        wal_segment_max_bytes: 8 * 1024,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

/// Total bytes across the active `journal.wal` and every archived
/// `journal.<n>.wal` segment, and the archived-segment count.
fn wal_bytes_and_segments(dir: &Path) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut archived = 0usize;
    for e in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let is_active = name == "journal.wal";
        let is_archived = name
            .strip_prefix("journal.")
            .and_then(|s| s.strip_suffix(".wal"))
            .and_then(|m| m.parse::<u64>().ok())
            .is_some();
        if is_active || is_archived {
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            if is_archived {
                archived += 1;
            }
        }
    }
    (bytes, archived)
}

#[test]
fn graceful_shutdown_reclaims_the_wal_and_recovery_replays_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let n: u64 = 400;
    let padding = "x".repeat(400); // fat values so the 8 KiB segments roll fast

    // 1. Write without flushing. The spatial memtable holds every write, so its
    //    manifest-durable seqno stays at its start value and the pruner cannot
    //    drop the archived segments — the WAL is genuinely un-pruned at shutdown.
    {
        let engine = TurbaEngine::open(tmp.path(), config()).expect("open");
        for i in 0..n {
            let key = format!("key{i:05}");
            let value = format!("v{i}-{padding}");
            let mut batch = engine.batch();
            batch.put_spatial(key.as_bytes(), value.as_bytes());
            batch.commit().expect("commit");
        }

        // Precondition: the WAL really is segmented + non-trivial before
        // shutdown, so the assertion below tests reclaim, not an empty WAL.
        let (bytes_before, segs_before) = wal_bytes_and_segments(tmp.path());
        assert!(
            segs_before >= 2 && bytes_before > 64 * 1024,
            "expected a segmented, un-pruned WAL before shutdown \
             (got {segs_before} archived segments, {bytes_before} bytes)"
        );

        // 2. Graceful shutdown must reclaim the WAL.
        engine.shutdown().expect("shutdown");
    }

    // 3. Post-shutdown: only SSTables remain. The active WAL is empty and no
    //    archived segments survive. A refactor that drops the shutdown-time
    //    rotate (deuda #10) fails HERE.
    let (bytes_after, segs_after) = wal_bytes_and_segments(tmp.path());
    assert_eq!(
        (bytes_after, segs_after),
        (0, 0),
        "graceful shutdown must reclaim the WAL (found {bytes_after} bytes, \
         {segs_after} archived segments) — a non-empty WAL here means recovery \
         would replay the full history (deuda #10: 2x disk + OOM on restart)"
    );

    // 4. Data survives: recovery reads it from SSTables (the WAL is empty, so it
    //    replays nothing) and every acked record is present.
    let engine = TurbaEngine::open(tmp.path(), config()).expect("reopen");
    let mut recovered = 0u64;
    for i in 0..n {
        let key = format!("key{i:05}");
        if engine.spatial.get(key.as_bytes()).expect("get").is_some() {
            recovered += 1;
        }
    }
    assert_eq!(
        recovered, n,
        "every acked record must survive a WAL-reclaiming shutdown \
         ({n} written, {recovered} recovered)"
    );
}
