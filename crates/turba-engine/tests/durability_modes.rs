//! 5a/5b — durability modes (SyncData vs Buffer). The real risk is FALSE
//! durability in Buffer/async mode, so the repros target the contract, not
//! the happy path.
//!
//! turba has two modes: SyncData (every commit fsynced via the group-commit
//! barrier — acked == durable) and Buffer (the commit returns once the bytes
//! are in the in-memory WAL BufWriter — acked != durable; the OS, shutdown(),
//! or Drop flush later). There is NO periodic fsync thread for Buffer (the
//! sync thread is spawned only for SyncData), so its window is bounded by OS
//! writeback / shutdown, not by turba.
//!
//! Contract under test:
//!   5b (critical): a CLEAN shutdown() must flush the Buffer window — every
//!       acked write survives a restart. A clean shutdown that loses acked
//!       data is unacceptable. (Drop is a best-effort backstop, 3a'; the
//!       contract is shutdown().)
//!   5a: Buffer ack != durable — a crash (no shutdown) loses the unflushed
//!       window (documented, bounded to the unflushed writes), while a
//!       SyncData crash loses nothing.

use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

fn config(mode: PersistMode) -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: mode,
        wal_path: None,
        wal_segment_max_bytes: 64 * 1024 * 1024,
        worker_threads: 1,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

/// 5b — a clean shutdown() flushes the Buffer-mode window: every acked write
/// survives the restart, even though none was fsynced at commit time.
#[test]
fn buffer_mode_clean_shutdown_preserves_all_acked() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");

    {
        let engine = TurbaEngine::open(&db, config(PersistMode::Buffer)).unwrap();
        for i in 0..20u32 {
            let mut b = engine.batch();
            b.put_spatial(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes());
            // Acked, but NOT fsynced (Buffer): only buffered in the WAL writer.
            b.commit().unwrap();
        }
        // The contract: a clean shutdown flushes + fsyncs the window.
        engine.shutdown().expect("clean shutdown must succeed");
        // Data is durable via shutdown's sync; forget so Drop does no extra work.
        engine._test_release_dir_lock();
        std::mem::forget(engine);
    }

    let engine = TurbaEngine::open(&db, config(PersistMode::Buffer)).unwrap();
    for i in 0..20u32 {
        assert_eq!(
            engine
                .spatial
                .get(format!("k{i:04}").as_bytes())
                .unwrap()
                .as_deref(),
            Some(format!("v{i}").as_bytes()),
            "Buffer-mode acked write k{i:04} lost across a CLEAN shutdown (5b — unacceptable)"
        );
    }
}

/// 5a — the durability contract distinguishes the modes under a CRASH (no
/// shutdown): SyncData acked == durable (survives), Buffer acked != durable
/// (the unflushed window is lost — documented, and bounded to the unflushed
/// writes, never the whole store).
#[test]
fn crash_without_shutdown_honours_each_mode_contract() {
    // SyncData: every acked write is fsynced at commit → survives a crash.
    {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("db");
        {
            let engine = TurbaEngine::open(&db, config(PersistMode::SyncData)).unwrap();
            for i in 0..10u32 {
                let mut b = engine.batch();
                b.put_spatial(format!("s{i}").as_bytes(), b"x");
                b.commit().unwrap();
            }
            engine._test_release_dir_lock();
            std::mem::forget(engine); // SIGKILL, no shutdown
        }
        let engine = TurbaEngine::open(&db, config(PersistMode::SyncData)).unwrap();
        for i in 0..10u32 {
            assert!(
                engine
                    .spatial
                    .get(format!("s{i}").as_bytes())
                    .unwrap()
                    .is_some(),
                "SyncData acked write s{i} lost after a crash — durability contract violated"
            );
        }
    }

    // Buffer: acks are not fsynced, so a crash without shutdown loses the
    // unflushed window. This is the documented Buffer contract — assert it so
    // a future change that silently makes Buffer behave like SyncData (or
    // vice-versa) is caught.
    {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("db");
        {
            let engine = TurbaEngine::open(&db, config(PersistMode::Buffer)).unwrap();
            for i in 0..10u32 {
                let mut b = engine.batch();
                b.put_spatial(format!("b{i}").as_bytes(), b"x");
                b.commit().unwrap();
            }
            engine._test_release_dir_lock();
            std::mem::forget(engine); // SIGKILL, no shutdown, no flush
        }
        let engine = TurbaEngine::open(&db, config(PersistMode::Buffer)).unwrap();
        let recovered = (0..10u32)
            .filter(|i| {
                engine
                    .spatial
                    .get(format!("b{i}").as_bytes())
                    .unwrap()
                    .is_some()
            })
            .count();
        assert_eq!(
            recovered, 0,
            "Buffer-mode writes were unexpectedly durable across a crash without shutdown \
             (the WAL writer buffers in memory; a crash must lose the unflushed window) — \
             got {recovered}/10. If Buffer was changed to fsync, update this contract test."
        );
    }
}
