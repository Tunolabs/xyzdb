use std::sync::Arc;
use turba_engine::config::{EngineConfig, IoSchedulerMode};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

fn test_engine() -> (TurbaEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        cache_size_bytes: 32 * 1024 * 1024,
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };
    let engine = TurbaEngine::open(&dir.path().join("db"), config).unwrap();
    (engine, dir)
}

// --- Basic open/close/reopen ---

#[test]
fn engine_open_close_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    // Phase 1: write data
    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        let mut batch = engine.batch();
        batch.put_spatial(b"s_key1", b"s_val1");
        batch.put_identity(b"i_key1", b"i_val1");
        batch.put_dictionary(b"d_key1", b"d_val1");
        batch.commit().unwrap();
        engine.shutdown().unwrap();
    }

    // Phase 2: reopen — data recovered from WAL + SSTables
    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        assert_eq!(
            engine.spatial.get(b"s_key1").unwrap(),
            Some(b"s_val1".to_vec())
        );
        assert_eq!(
            engine.identity.get(b"i_key1").unwrap(),
            Some(b"i_val1".to_vec())
        );
        assert_eq!(
            engine.dictionary.get(b"d_key1").unwrap(),
            Some(b"d_val1".to_vec())
        );
    }
}

// --- Batch atomicity ---

#[test]
fn engine_batch_atomic() {
    let (engine, _dir) = test_engine();

    let mut batch = engine.batch();
    batch.put_spatial(b"sk", b"sv");
    batch.put_identity(b"ik", b"iv");
    batch.put_dictionary(b"dk", b"dv");
    batch.put_ghosts(b"gk", b"gv");
    let seqno = batch.commit().unwrap();

    assert!(seqno > 0);
    assert_eq!(engine.spatial.get(b"sk").unwrap(), Some(b"sv".to_vec()));
    assert_eq!(engine.identity.get(b"ik").unwrap(), Some(b"iv".to_vec()));
    assert_eq!(engine.dictionary.get(b"dk").unwrap(), Some(b"dv".to_vec()));
    assert_eq!(engine.ghosts.get(b"gk").unwrap(), Some(b"gv".to_vec()));
}

// --- Crash recovery: complete batch survives ---

#[test]
fn engine_crash_recovery_complete_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    // Write batches with sync — simulates "crash after write"
    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        for i in 0..10u32 {
            let mut batch = engine.batch();
            batch.put_spatial(
                format!("key_{i:04}").as_bytes(),
                format!("val_{i}").as_bytes(),
            );
            batch.commit().unwrap();
        }
        // "Crash" — drop without shutdown (WAL not truncated)
    }

    // Recovery: all 10 batches should be replayed
    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        for i in 0..10u32 {
            let key = format!("key_{i:04}");
            let val = engine.spatial.get(key.as_bytes()).unwrap();
            assert!(val.is_some(), "missing {key} after recovery");
            assert_eq!(val.unwrap(), format!("val_{i}").as_bytes());
        }
    }
}

// --- Crash recovery: incomplete batch discarded ---

#[test]
fn engine_crash_recovery_incomplete_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    // Write a complete batch
    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        let mut batch = engine.batch();
        batch.put_spatial(b"good_key", b"good_val");
        batch.commit().unwrap();
        // Don't shutdown — "crash"
    }

    // Append an incomplete batch directly to the WAL (simulates partial write before crash)
    {
        let journal_path = db_path.join("journal.wal");
        let mut data = std::fs::read(&journal_path).unwrap();
        // Append a Start tag + partial item (no End tag)
        data.push(1); // TAG_START
        data.extend_from_slice(&1u32.to_le_bytes()); // item_count
        data.extend_from_slice(&999u64.to_le_bytes()); // seqno
        data.push(2); // TAG_ITEM
        data.push(0); // keyspace_id
        data.extend_from_slice(&3u32.to_le_bytes()); // key_len
        data.extend_from_slice(b"bad"); // key
        // Truncated here — no value, no End tag
        std::fs::write(&journal_path, &data).unwrap();
    }

    // Recovery: good batch survives, incomplete batch discarded
    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        assert_eq!(
            engine.spatial.get(b"good_key").unwrap(),
            Some(b"good_val".to_vec()),
            "complete batch should survive"
        );
        assert_eq!(
            engine.spatial.get(b"bad").unwrap(),
            None,
            "incomplete batch should be discarded"
        );
    }
}

// --- Crash during flush: no corruption ---

#[test]
fn engine_crash_during_flush_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    // Write data and flush manually
    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        for i in 0..200u32 {
            let mut batch = engine.batch();
            batch.put_spatial(
                format!("key_{i:04}").as_bytes(),
                format!("val_{i}").as_bytes(),
            );
            batch.commit().unwrap();
        }
        // Flush spatial
        engine.spatial.seal_active();
        engine.spatial.flush_sealed().unwrap();
        // "Crash" — drop without full shutdown
    }

    // Reopen — data from SSTables (flushed) + WAL replay
    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        for i in 0..200u32 {
            let key = format!("key_{i:04}");
            let val = engine.spatial.get(key.as_bytes()).unwrap();
            assert!(val.is_some(), "missing {key} after flush + crash recovery");
        }
    }
}

// --- Concurrent 4-keyspace writes ---

#[test]
fn engine_concurrent_4_keyspaces() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig::default();
    let engine = Arc::new(TurbaEngine::open(&dir.path().join("db"), config).unwrap());

    let mut handles = Vec::new();

    for ks_id in 0..4u8 {
        let engine = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            for i in 0..500u32 {
                let mut batch = engine.batch();
                let key = format!("k{ks_id}_{i:04}");
                let val = format!("v{i}");
                match ks_id {
                    0 => batch.put_spatial(key.as_bytes(), val.as_bytes()),
                    1 => batch.put_identity(key.as_bytes(), val.as_bytes()),
                    2 => batch.put_dictionary(key.as_bytes(), val.as_bytes()),
                    3 => batch.put_ghosts(key.as_bytes(), val.as_bytes()),
                    _ => unreachable!(),
                }
                batch.commit().unwrap();
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Verify all writes
    for i in 0..500u32 {
        assert!(
            engine
                .spatial
                .get(format!("k0_{i:04}").as_bytes())
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .identity
                .get(format!("k1_{i:04}").as_bytes())
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .dictionary
                .get(format!("k2_{i:04}").as_bytes())
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .ghosts
                .get(format!("k3_{i:04}").as_bytes())
                .unwrap()
                .is_some()
        );
    }
}

// --- 1K records CRUD cycle ---

#[test]
fn engine_1k_records_crud() {
    let (engine, _dir) = test_engine();

    // Create
    for i in 0..1000u32 {
        let mut batch = engine.batch();
        batch.put_spatial(
            format!("rec_{i:06}").as_bytes(),
            format!("data_{i}").as_bytes(),
        );
        batch.put_identity(
            format!("lid_{i:06}").as_bytes(),
            format!("rec_{i:06}").as_bytes(),
        );
        batch.commit().unwrap();
    }

    // Read
    for i in 0..1000u32 {
        assert!(
            engine
                .spatial
                .get(format!("rec_{i:06}").as_bytes())
                .unwrap()
                .is_some()
        );
    }

    // Update (overwrite)
    for i in 0..100u32 {
        let mut batch = engine.batch();
        batch.put_spatial(
            format!("rec_{i:06}").as_bytes(),
            format!("updated_{i}").as_bytes(),
        );
        batch.commit().unwrap();
    }
    assert_eq!(
        engine.spatial.get(b"rec_000050").unwrap(),
        Some(b"updated_50".to_vec())
    );

    // Delete
    for i in 0..50u32 {
        let mut batch = engine.batch();
        batch.remove_spatial(format!("rec_{i:06}").as_bytes());
        batch.remove_identity(format!("lid_{i:06}").as_bytes());
        batch.commit().unwrap();
    }
    assert_eq!(engine.spatial.get(b"rec_000025").unwrap(), None);
    assert!(engine.spatial.get(b"rec_000500").unwrap().is_some());
}

// --- WAL replay with 100 batches ---

#[test]
fn engine_wal_replay_100_batches() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        for i in 0..100u32 {
            let mut batch = engine.batch();
            batch.put_spatial(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes());
            batch.put_identity(format!("i{i:04}").as_bytes(), format!("x{i}").as_bytes());
            batch.commit().unwrap();
        }
        // "Crash" — no shutdown
    }

    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        for i in 0..100u32 {
            assert!(
                engine
                    .spatial
                    .get(format!("k{i:04}").as_bytes())
                    .unwrap()
                    .is_some(),
                "spatial key k{i:04} missing after WAL replay"
            );
            assert!(
                engine
                    .identity
                    .get(format!("i{i:04}").as_bytes())
                    .unwrap()
                    .is_some(),
                "identity key i{i:04} missing after WAL replay"
            );
        }
    }
}

// --- Backpressure doesn't crash ---

#[test]
fn engine_backpressure_under_load() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig::default();
    let engine = TurbaEngine::open(&dir.path().join("db"), config).unwrap();

    // Flood with writes — backpressure should slow but not crash
    for i in 0..5000u32 {
        let mut batch = engine.batch();
        batch.put_spatial(
            format!("flood_{i:06}").as_bytes(),
            format!("data_{i}_padding_to_increase_size").as_bytes(),
        );
        batch.commit().unwrap();
    }

    // All data should be present
    assert!(engine.spatial.get(b"flood_000000").unwrap().is_some());
    assert!(engine.spatial.get(b"flood_004999").unwrap().is_some());
}

// --- Cross-keyspace batch with deletes ---

#[test]
fn engine_batch_with_deletes() {
    let (engine, _dir) = test_engine();

    // Insert
    let mut batch = engine.batch();
    batch.put_spatial(b"sk1", b"sv1");
    batch.put_spatial(b"sk2", b"sv2");
    batch.put_identity(b"ik1", b"iv1");
    batch.commit().unwrap();

    // Delete in batch
    let mut batch = engine.batch();
    batch.remove_spatial(b"sk1");
    batch.remove_identity(b"ik1");
    batch.commit().unwrap();

    assert_eq!(engine.spatial.get(b"sk1").unwrap(), None);
    assert_eq!(engine.spatial.get(b"sk2").unwrap(), Some(b"sv2".to_vec()));
    assert_eq!(engine.identity.get(b"ik1").unwrap(), None);
}

// --- Shutdown + reopen consistency ---

#[test]
fn engine_shutdown_reopen_consistency() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        for i in 0..500u32 {
            let mut batch = engine.batch();
            batch.put_spatial(format!("k{i:04}").as_bytes(), format!("v{i}").as_bytes());
            batch.commit().unwrap();
        }
        engine.shutdown().unwrap(); // clean shutdown flushes everything
    }

    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        for i in 0..500u32 {
            let key = format!("k{i:04}");
            assert!(
                engine.spatial.get(key.as_bytes()).unwrap().is_some(),
                "missing {key} after shutdown+reopen"
            );
        }
    }
}

// --- scheduler wiring (post enforce-ladder retirement) ---

/// Hdd + default config -> laned scheduler (observability only; ladder
/// retired in v0.5 per DEC-V5-11).
#[test]
fn hdd_profile_selects_laned_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        io_scheduler: IoSchedulerMode::Hdd,
        ..Default::default()
    };
    let engine = TurbaEngine::open(&dir.path().join("db"), config).unwrap();
    let scheduler = engine.spatial.scheduler();
    assert_eq!(scheduler.mode_str(), "laned");
}

/// Ssd profile -> Passthrough scheduler regardless of other config.
#[test]
fn ssd_profile_selects_passthrough_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        io_scheduler: IoSchedulerMode::Ssd,
        ..Default::default()
    };
    let engine = TurbaEngine::open(&dir.path().join("db"), config).unwrap();
    let scheduler = engine.spatial.scheduler();
    assert_eq!(scheduler.mode_str(), "passthrough");
}

/// All five trees must share the same Arc<Scheduler>. Guards against a
/// future change that splits scheduler construction per-tree.
#[test]
fn scheduler_shared_across_all_trees() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig {
        io_scheduler: IoSchedulerMode::Hdd,
        ..Default::default()
    };
    let engine = TurbaEngine::open(&dir.path().join("db"), config).unwrap();
    assert!(
        Arc::ptr_eq(engine.spatial.scheduler(), engine.identity.scheduler()),
        "All trees must share the same Scheduler instance"
    );
    assert!(Arc::ptr_eq(
        engine.spatial.scheduler(),
        engine.dictionary.scheduler()
    ));
    assert!(Arc::ptr_eq(
        engine.spatial.scheduler(),
        engine.ghosts.scheduler()
    ));
    assert!(Arc::ptr_eq(
        engine.spatial.scheduler(),
        engine.vectors.scheduler()
    ));
}

// --- v0.5.2 B.5: --wal-path standalone ---

/// `EngineConfig::wal_path = None` keeps the historical
/// `<path>/journal.wal` co-location.
#[test]
fn wal_path_default_keeps_journal_inside_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };
    let engine = TurbaEngine::open(&db_path, config).unwrap();
    let resolved = engine.wal_path().to_path_buf();
    let expected = db_path.join("journal.wal");
    assert_eq!(
        resolved, expected,
        "default WAL path must equal <path>/journal.wal"
    );
    // Force a write so the file exists.
    let mut batch = engine.batch();
    batch.put_spatial(b"k", b"v");
    batch.commit().unwrap();
    drop(engine);
    assert!(
        expected.exists(),
        "journal.wal must be written under db dir"
    );
}

/// `EngineConfig::wal_path = Some(p)` relocates the WAL to `p`.
#[test]
fn wal_path_override_relocates_journal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let wal_path = dir.path().join("wal_dir").join("custom.wal");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        wal_path: Some(wal_path.clone()),
        ..Default::default()
    };
    let engine = TurbaEngine::open(&db_path, config).unwrap();
    let resolved = engine.wal_path().to_path_buf();
    assert_eq!(resolved, wal_path);
    let mut batch = engine.batch();
    batch.put_spatial(b"k1", b"v1");
    batch.commit().unwrap();
    drop(engine);
    assert!(
        wal_path.exists(),
        "WAL must be written at the override path"
    );
    assert!(
        !db_path.join("journal.wal").exists(),
        "data dir must NOT contain a journal.wal when wal_path is set"
    );
}

// --- 5th keyspace: vectors ---

/// The `vectors` keyspace behaves as a first-class LSM keyspace: writes via
/// WriteBatch commit, reads back, survive a drop/reopen WAL recovery, and do
/// not disturb the other keyspaces.
#[test]
fn vectors_keyspace_roundtrip_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        ..Default::default()
    };

    // Phase 1: write a few vectors keys plus a spatial control key.
    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        let mut batch = engine.batch();
        batch.put_vectors(b"vec_key1", b"vec_val1");
        batch.put_vectors(b"vec_key2", b"vec_val2");
        batch.put_vectors(b"vec_key3", b"vec_val3");
        batch.put_spatial(b"s_ctrl", b"s_ctrl_val");
        batch.commit().unwrap();

        // Read back from both keyspaces in the same process.
        assert_eq!(
            engine.vectors.get(b"vec_key1").unwrap(),
            Some(b"vec_val1".to_vec())
        );
        assert_eq!(
            engine.vectors.get(b"vec_key2").unwrap(),
            Some(b"vec_val2".to_vec())
        );
        assert_eq!(
            engine.vectors.get(b"vec_key3").unwrap(),
            Some(b"vec_val3".to_vec())
        );
        assert_eq!(
            engine.spatial.get(b"s_ctrl").unwrap(),
            Some(b"s_ctrl_val".to_vec())
        );
        // Drop without shutdown — forces WAL recovery on reopen.
    }

    // Phase 2: reopen — vectors keys recovered from the WAL.
    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        assert_eq!(
            engine.vectors.get(b"vec_key1").unwrap(),
            Some(b"vec_val1".to_vec()),
            "vectors key must survive WAL recovery"
        );
        assert_eq!(
            engine.vectors.get(b"vec_key2").unwrap(),
            Some(b"vec_val2".to_vec())
        );
        assert_eq!(
            engine.vectors.get(b"vec_key3").unwrap(),
            Some(b"vec_val3".to_vec())
        );

        // The spatial control key is unaffected; the other keyspaces stay
        // empty (no cross-keyspace bleed).
        assert_eq!(
            engine.spatial.get(b"s_ctrl").unwrap(),
            Some(b"s_ctrl_val".to_vec()),
            "spatial control key must be unaffected"
        );
        assert_eq!(engine.identity.get(b"vec_key1").unwrap(), None);
        assert_eq!(engine.dictionary.get(b"vec_key1").unwrap(), None);
        assert_eq!(engine.ghosts.get(b"vec_key1").unwrap(), None);
        // A vectors key must not leak into spatial.
        assert_eq!(engine.spatial.get(b"vec_key1").unwrap(), None);
    }
}

/// Reopening the engine with the same `wal_path` recovers data
/// written before shutdown — the override path round-trips.
#[test]
fn wal_path_override_round_trips_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let wal_path = dir.path().join("wal_alt.wal");
    let config = EngineConfig {
        persist_mode: PersistMode::SyncData,
        wal_path: Some(wal_path.clone()),
        ..Default::default()
    };
    // First open: write a record.
    {
        let engine = TurbaEngine::open(&db_path, config.clone()).unwrap();
        let mut batch = engine.batch();
        batch.put_spatial(b"persist", b"value");
        batch.commit().unwrap();
        // Drop without explicit shutdown — WAL recovery exercises.
    }
    // Reopen: data should be present.
    {
        let engine = TurbaEngine::open(&db_path, config).unwrap();
        let got = engine.spatial.get(b"persist").unwrap();
        assert_eq!(
            got,
            Some(b"value".to_vec()),
            "record must survive reopen via the override WAL path"
        );
    }
}
