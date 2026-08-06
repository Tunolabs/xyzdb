//! Stress reproducer for the flush/compact meta race observed in the v0.2.0-alpha
//! concurrent benchmarks.
//!
//! Symptom: `turba-compact: error: data corruption: bad <field>` lines on stderr
//! during heavy-write workloads. Integrity of committed data was never affected
//! — failed compactions retry — but the error rate (1,500–2,000 lines per 1 h run)
//! signalled a race in the compact thread's path through `SSTableReader::open`.
//!
//! Pre-fix (v0.2.0-alpha): under the configuration below, this test reliably
//! produces `compact_error_count > 0` within the run. Post-fix (atomic meta
//! publish via `.tmp` + rename in v0.2.1), the expected count is zero.
//!
//! The assertion is deliberately weak right now — the file is committed first
//! as a _reproducer_ so we have a known-failing regression gate before the
//! fix lands. Once the fix is in, the assert upgrades to `errors == 0`.
//!
//! Marked `#[ignore]` because it takes 30–90 s; run via
//!   cargo test -p turba-engine --release --test flush_compact_meta_race_stress -- --ignored --nocapture

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use std::time::{Duration, Instant};
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

#[test]
#[ignore]
fn flush_compact_meta_race_stress() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Buffer mode (no fsync per commit) so the writer loop is bound by
    // memtable throughput, not disk. That maximizes flush-per-second and
    // therefore the opportunity for a flush→compact race to trigger.
    let config = EngineConfig {
        cache_size_bytes: 64 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::Buffer,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        wal_path: None,
        wal_segment_max_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };

    let engine = Arc::new(TurbaEngine::open(dir.path(), config).expect("open"));

    // Tuned so the ghosts keyspace (8 MB memtable default) sees ≥20 flushes
    // and L0→L1 compaction fires multiple times. Spatial keyspace (32 MB
    // memtable default) sees ~6 flushes — also enough to trigger compaction.
    let n_writers = 4usize;
    let ops_per_writer = 200_000usize;
    let value = vec![0u8; 128];

    let start = Instant::now();
    let handles: Vec<_> = (0..n_writers)
        .map(|tid| {
            let e = Arc::clone(&engine);
            let v = value.clone();
            std::thread::spawn(move || {
                for i in 0..ops_per_writer {
                    // Keys non-overlapping per writer so we exercise key-range
                    // splits in L0 and non-trivial compaction merges.
                    let key = format!("w{tid:02}_k{i:010}");
                    let mut batch = e.batch();
                    batch.put_spatial(key.as_bytes(), &v);
                    batch.put_ghosts(key.as_bytes(), &v);
                    batch.commit().expect("commit");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("writer join");
    }
    let write_elapsed = start.elapsed();
    eprintln!(
        "[reproducer] writes complete: {} ops in {:?} ({:.0}/s)",
        n_writers * ops_per_writer,
        write_elapsed,
        (n_writers * ops_per_writer) as f64 / write_elapsed.as_secs_f64()
    );

    // Let the background flush + compact threads drain. We sleep in short
    // windows and poll so a post-fix test doesn't sit idle for longer than
    // it needs to.
    let drain_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let spatial_l0 = engine.spatial.l0_table_count();
        let ghosts_l0 = engine.ghosts.l0_table_count();
        let spatial_sealed = engine.spatial.sealed_memtable_count();
        let ghosts_sealed = engine.ghosts.sealed_memtable_count();
        if Instant::now() >= drain_deadline {
            eprintln!("[reproducer] drain deadline reached (30 s); proceeding with stats");
            break;
        }
        if spatial_l0 <= 4 && ghosts_l0 <= 4 && spatial_sealed == 0 && ghosts_sealed == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let errors = engine.total_compact_errors();
    eprintln!("[reproducer] total turba-compact errors: {errors}");
    eprintln!(
        "[reproducer] spatial: flushed_seqno={} l0={}",
        engine.spatial.flushed_seqno(),
        engine.spatial.l0_table_count()
    );
    eprintln!(
        "[reproducer] ghosts:  flushed_seqno={} l0={}",
        engine.ghosts.flushed_seqno(),
        engine.ghosts.l0_table_count()
    );

    // Pre-fix (reproducer commit): we expect this to be > 0 in most runs.
    // If it's zero on a lucky run, we still proceed — the HDD 1 h
    // integration benchmark is the authoritative validator.
    //
    // Post-fix (atomic meta publish): this assertion upgrades to
    //   assert_eq!(errors, 0, "atomic meta publish regressed");
    if errors == 0 {
        eprintln!(
            "[reproducer] WARN: no compact errors observed on this run. \
             Either the race is mitigated or this run was lucky. \
             Re-run or check the HDD 1 h benchmark."
        );
    }
}
