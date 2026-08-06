//! Ticket 2 (0.9.2) durability regression + class guard: COMPACT must not drop
//! ANY keyspace's unflushed tail when it truncates the WAL.
//!
//! `xyzdb-engine::execute_compact` seals + major-compacts every keyspace and
//! then calls `rotate_journal()`, which truncates the WAL. `rotate()` truncates
//! UNCONDITIONALLY (`journal/writer.rs::rotate`), so its safety rests entirely
//! on the precondition "all acked data is already in SSTs". The original bug:
//! execute_compact flushed spatial/identity/dictionary (+ghosts) but NOT the
//! `vectors` keyspace, which a vector PUT co-commits with `spatial` in one batch
//! (same WAL seqno) — so after COMPACT the acked vector lived only in the
//! vectors active memtable and was lost on crash.
//!
//! Two tests, closing the CLASS, not just the vectors instance:
//! 1. Every keyspace's unflushed acked value survives COMPACT + crash.
//! 2. `rotate_journal` REFUSES (loud error, no truncation) when any keyspace
//!    lags — so a future maintenance op that flushes only a subset can never
//!    silently drop the rest.

// SPDX-License-Identifier: BUSL-1.1
use tempfile::TempDir;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

fn make_config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::SyncData,
        wal_path: None,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

/// Enumerative: an acked-but-unflushed value in EVERY keyspace COMPACT flushes
/// (spatial/identity/dictionary/vectors, all co-committable in one batch) must
/// survive COMPACT + crash. Replicates execute_compact's FIXED turba sequence
/// (seal + major_compact every tree, then rotate). Drop any keyspace from the
/// seal set and either its value is lost or rotate_journal refuses — both red.
#[test]
fn compact_preserves_every_keyspace_across_crash() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let key = b"k0001".to_vec();
    let vs = vec![1u8; 32]; // spatial
    let vi = vec![2u8; 32]; // identity
    let vd = vec![3u8; 32]; // dictionary
    let vv = vec![4u8; 64]; // vectors

    {
        let engine = TurbaEngine::open(&db_path, make_config()).unwrap();

        // One atomic batch touches all four keyspaces at the same WAL seqno,
        // as a vector PUT does (ops/put.rs: put_spatial + put_vectors, plus the
        // LID/field-registry co-commits into identity/dictionary).
        let mut bt = engine.batch();
        bt.put_spatial(&key, &vs);
        bt.put_identity(&key, &vi);
        bt.put_dictionary(&key, &vd);
        bt.put_vectors(&key, &vv);
        bt.commit().unwrap();

        // execute_compact's FIXED sequence: seal + major_compact EVERY tree.
        for t in [
            &engine.spatial,
            &engine.identity,
            &engine.dictionary,
            &engine.vectors,
        ] {
            t.seal_active();
            t.major_compact().unwrap();
        }
        engine.rotate_journal().unwrap();

        // Crash: stop bg threads without flushing, then leak (real SIGKILL).
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
    for (ks, got, want) in [
        ("spatial", engine.spatial.get(&key).unwrap(), &vs),
        ("identity", engine.identity.get(&key).unwrap(), &vi),
        ("dictionary", engine.dictionary.get(&key).unwrap(), &vd),
        ("vectors", engine.vectors.get(&key).unwrap(), &vv),
    ] {
        assert_eq!(
            got.as_deref(),
            Some(want.as_slice()),
            "{ks} value lost after COMPACT+crash — keyspace dropped from the seal set before rotate"
        );
    }
}

/// Class guard: `rotate_journal` truncates the WAL, so it MUST refuse when any
/// keyspace still holds an acked-but-unflushed tail — otherwise a maintenance
/// op that flushed only a SUBSET (the compact-skips-vectors bug) silently drops
/// the rest. Proven generically via the vectors keyspace: it lags (unsealed),
/// so rotate is refused with a loud error AND the acked write survives a crash
/// because the WAL was never truncated.
#[test]
fn rotate_journal_refuses_when_a_keyspace_lags() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let key = b"k0001".to_vec();
    let vv = vec![7u8; 64];

    {
        let engine = TurbaEngine::open(&db_path, make_config()).unwrap();

        let mut bt = engine.batch();
        bt.put_vectors(&key, &vv);
        bt.commit().unwrap();

        // vectors is NOT sealed/flushed → it lags. rotate_journal must refuse
        // rather than truncate the WAL and strand the acked vector in RAM.
        let err = engine.rotate_journal().unwrap_err();
        assert!(
            matches!(err, turba_engine::error::Error::WalRotatePrecondition(_)),
            "rotate_journal must refuse (WalRotatePrecondition) when a keyspace lags; got {err:?}"
        );

        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    // The refused rotate left the WAL intact → the acked vector is recovered.
    let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
    assert_eq!(
        engine.vectors.get(&key).unwrap().as_deref(),
        Some(vv.as_slice()),
        "acked vector lost after a REFUSED rotate — the guard must not truncate the WAL"
    );
}
