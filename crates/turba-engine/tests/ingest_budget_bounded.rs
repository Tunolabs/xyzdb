//! Ingest is bounded by the memory budget.
//!
//! A real PUT fans out across several keyspaces (record → spatial, embedding →
//! vectors, anchor → dictionary, id → identity). Before the budget-aware ingest
//! backpressure, the summed active+sealed memtable bytes across those keyspaces
//! could grow far past a tight container's limit — the T1/246k agentic OOM
//! (build died at 244 MB under a 256 MB budget). This test drives that fan-out
//! at a tight budget and asserts the summed footprint stays within the derived
//! ceiling: the writer stalls for background flush instead of ballooning.

use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;
use turba_engine::memory_budget::memtable_ceiling_from_budget;

#[test]
fn ingest_stays_within_budget_ceiling() {
    let dir = TempDir::new().unwrap();
    let budget = 64 * 1024 * 1024u64; // tight: ceiling ≈ 22.4 MiB
    let cfg = EngineConfig {
        cache_size_bytes: 8 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        // Buffer: skip per-commit fsync so the load is fast — we are exercising
        // memtable memory bounding, not durability. Backpressure is independent
        // of the persist mode.
        persist_mode: PersistMode::Buffer,
        wal_path: None,
        worker_threads: 2,
        io_scheduler: IoSchedulerMode::Ssd,
        memory_budget_bytes: budget,
        ..Default::default()
    };
    let engine = TurbaEngine::open(&dir.path().join("db"), cfg).unwrap();
    let ceiling = memtable_ceiling_from_budget(budget) as usize;

    // ~120 MB of logical data (≈ 5× the ceiling) fanned across 4 keyspaces —
    // enough to force many drain cycles through the stall.
    let val = vec![0u8; 1024];
    let mut peak = 0usize;
    for i in 0..30_000u32 {
        let k = i.to_be_bytes();
        let mut b = engine.batch();
        b.put_spatial(&k, &val);
        b.put_identity(&k, &val);
        b.put_dictionary(&k, &val);
        b.put_vectors(&k, &val);
        b.commit().unwrap();
        peak = peak.max(engine.global_memtable_bytes());
    }

    // The stall holds the summed footprint near the ceiling. 2× slack covers the
    // post-insert overshoot (backpressure is checked AFTER the commit) plus one
    // in-flight seal per keyspace. Without the fix, peak would track the whole
    // dataset — many times the ceiling.
    assert!(
        peak <= ceiling * 2,
        "ingest peak {peak} B exceeded 2x ceiling {ceiling} B (budget {budget} B) — \
         global stall is not bounding memtable growth"
    );
    engine.shutdown().unwrap();
}
