//! Mechanism guard for the deuda #10 flush-only checkpoint: `Tree::checkpoint_flush`
//! must FLUSH the memtable + persist the manifest WITHOUT compacting — O(new data),
//! so the WAL pruner keeps pace under a high-scope load. A full `major_compact`
//! (the pruner's first, too-slow approach) re-reads the whole dataset every trigger
//! and falls behind, letting the WAL grow (crash-loop at a tight envelope).
//!
//! Deterministic + self-proving: it builds several L0 SSTables, then asserts in the
//! SAME run that `checkpoint_flush` PRESERVES them (flush-only) while `major_compact`
//! COLLAPSES them. If `checkpoint_flush` is ever changed to a full compaction the
//! first assertion fails — the test cannot go green with that regression.

// SPDX-License-Identifier: BUSL-1.1
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

fn config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 8 * 1024 * 1024,
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
fn checkpoint_flush_preserves_l0_but_major_compact_collapses_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let engine = TurbaEngine::open(tmp.path(), config()).expect("open");

    // Disable bg compaction so the L0 SSTables we build stay put — a deterministic
    // multi-SSTable state to compare the two checkpoint mechanisms against.
    engine.set_compaction_enabled(false);

    // Build one L0 SSTable per round: fill a sub-memtable batch, then flush it.
    let value = vec![b'q'; 4096];
    let per_round = 2000; // 2000 * 4KB = 8MB < 16MB memtable → one flush = one L0 SSTable
    for round in 0..4u64 {
        for i in 0..per_round {
            let key = format!("r{round}k{i:06}");
            let mut b = engine.batch();
            b.put_spatial(key.as_bytes(), &value);
            b.commit().expect("commit");
        }
        engine.spatial.seal_active();
        engine.spatial.flush_sealed().expect("flush");
    }
    let c0 = engine.spatial.l0_table_count();
    assert!(c0 >= 3, "need several L0 SSTables to compare (got {c0})");

    // Flush-only checkpoint must NOT COMPACT — it must never REDUCE the L0 count.
    // It may flush a residual active memtable (adding an L0 table), so the robust
    // invariant is `c1 >= c0`, not exact equality: how many L0 tables a flush
    // produces is timing-sensitive (sub-memtable batching, async flush settling)
    // and can vary by one on a contended runner. A compaction, by contrast, MERGES
    // L0 away (c < c0) — that reduction is the regression this guards against.
    engine.spatial.checkpoint_flush().expect("checkpoint_flush");
    let c1 = engine.spatial.l0_table_count();
    assert!(
        c1 >= c0,
        "checkpoint_flush must be flush-only; it reduced L0 {c0}->{c1}, i.e. it is compacting — \
         a full compaction cannot keep pace with the WAL under load (deuda #10)"
    );

    // A full major_compact DOES merge L0 away — proving the assertion above is
    // meaningful (the two mechanisms are genuinely distinguishable).
    engine.spatial.major_compact().expect("major_compact");
    let c2 = engine.spatial.l0_table_count();
    assert!(
        c2 < c1,
        "major_compact should collapse L0 ({c1}->{c2}); if it does not, the test cannot tell \
         flush-only from compaction"
    );

    engine.shutdown().expect("shutdown");
}
