//! Regression: the WAL stays bounded under sustained writes that build MANY
//! SSTables — the multi-scope pattern (deuda #10 intermediate). The pruner forces
//! a checkpoint once the WAL passes a memory-derived threshold; that checkpoint
//! MUST be flush-only (O(new data)). A full `major_compact` re-reads the whole
//! dataset every trigger, so under a load that has built hundreds of SSTables it
//! falls behind, the WAL grows with the full history, and a hard crash OOMs the
//! restart. This test writes fat (vector-sized) values with a small cap so the
//! checkpoint fires many times over a growing, many-SSTable dataset — it FAILS
//! (WAL unbounded ≈ full history) if the checkpoint is a full major_compact, and
//! PASSES (WAL ≈ one segment) with the flush-only checkpoint.
//!
//! Its own test binary: the process-wide `TURBA_WAL_MAX_BYTES` set below must not
//! race another test's engine open.

// SPDX-License-Identifier: BUSL-1.1
// This test measures the PRODUCTION WAL pruner — the size-triggered flush-only
// checkpoint that lives under `cfg(not(feature = "durability-test-hooks"))` in
// `engine.rs`. Under `--features durability-test-hooks` that pruner is replaced
// by the Finding-10 janitor (rotate-on-`flushed_seqno`), which does not bound the
// WAL this way, so the assertion below cannot hold. The behaviour under test only
// exists in the production configuration, so the whole binary compiles only there
// (`cargo test --workspace` exercises it; the `--features durability-test-hooks`
// run skips it). This is scoping, not `#[ignore]` and not a relaxed assertion.
#![cfg(not(feature = "durability-test-hooks"))]

use std::path::Path;
use std::time::Duration;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

const CAP: u64 = 512 * 1024; // forced reclaim threshold (512 KiB)

fn config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 8 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::SyncData,
        wal_path: None,
        wal_segment_max_bytes: 4 * 1024 * 1024, // 4 MiB segments → archived segments roll
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

fn wal_bytes(dir: &Path) -> u64 {
    let mut b = 0u64;
    for e in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let is_wal = name == "journal.wal"
            || name
                .strip_prefix("journal.")
                .and_then(|s| s.strip_suffix(".wal"))
                .and_then(|m| m.parse::<u64>().ok())
                .is_some();
        if is_wal {
            b += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    b
}

#[test]
fn wal_stays_bounded_under_many_sstable_load() {
    // SAFETY: single-threaded test setup; the var is set before the engine (and
    // its background threads) start, so there is no concurrent env access.
    unsafe {
        std::env::set_var("TURBA_WAL_MAX_BYTES", CAP.to_string());
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let n: u64 = 20_000;
    let value = vec![b'z'; 4096]; // vector-sized values → memtables fill often → many SSTables
    let total_written = n * 4200; // ≈ record size on the WAL; ~84 MB, far over 512 KiB

    {
        let engine = TurbaEngine::open(tmp.path(), config()).expect("open");
        for i in 0..n {
            let key = format!("key{i:07}");
            let mut batch = engine.batch();
            batch.put_spatial(key.as_bytes(), &value);
            batch.commit().expect("commit");
            // Every ~1500 writes, pause ~1.1 s so the ~1 s pruner runs a checkpoint
            // against the growing, many-SSTable dataset (where a full major_compact
            // can no longer keep pace).
            if i % 1500 == 1499 {
                std::thread::sleep(Duration::from_millis(1100));
            }
        }
        // Final checkpoint window.
        std::thread::sleep(Duration::from_millis(1500));
        let final_wal = wal_bytes(tmp.path());
        assert!(
            final_wal <= 12 * 1024 * 1024 && final_wal < total_written / 4,
            "WAL not bounded: {final_wal} bytes vs cap {CAP} (total written ~{total_written}); \
             a full major_compact checkpoint cannot keep pace with a many-SSTable load — \
             the pruner checkpoint must be flush-only (deuda #10)"
        );
        // Drop (not shutdown): leaves the bounded WAL on disk for recovery to replay.
        std::mem::drop(engine);
    }

    // Recovery replays the bounded WAL; every acked record must survive.
    let engine = TurbaEngine::open(tmp.path(), config()).expect("reopen");
    let mut recovered = 0u64;
    for i in 0..n {
        let key = format!("key{i:07}");
        if engine.spatial.get(key.as_bytes()).expect("get").is_some() {
            recovered += 1;
        }
    }
    engine.shutdown().expect("shutdown");
    // SAFETY: the engine has shut down; no other thread reads the environment.
    unsafe {
        std::env::remove_var("TURBA_WAL_MAX_BYTES");
    }
    assert_eq!(
        recovered, n,
        "every acked record must survive a bounded-WAL recovery ({n} written, {recovered} recovered)"
    );
}
