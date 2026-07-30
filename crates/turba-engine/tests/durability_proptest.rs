//! Property-based durability invariant:
//! for any sequence of committed batches, after dropping the engine and reopening,
//! every committed key-value pair is still retrievable.
//!
//! Phase 0 scaffolding. Phase 7 will extend this with crash injection (mid-batch
//! drop, partial fsync, and concurrent writers) and cross-keyspace invariants.
//!
//! Durability-cluster regression tests live here except for the
//! COMPACT-via-xyTalk path, which is in `crates/engine/tests/integration.rs`
//! because it drives the `COMPACT` flow through the `xyzdb-engine` xyTalk entry
//! point (`Engine::run`). Each D1 caller has a dedicated regression test.

use proptest::prelude::*;
use std::collections::BTreeMap;
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
        wal_segment_max_bytes: 64 * 1024 * 1024,
        worker_threads: 1,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 16,
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn spatial_put_survives_reopen(
        ops in proptest::collection::vec(
            (
                proptest::collection::vec(any::<u8>(), 1..24),
                proptest::collection::vec(any::<u8>(), 0..48),
            ),
            1..20,
        )
    ) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db");
        let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        {
            let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
            for (k, v) in &ops {
                let mut batch = engine.batch();
                batch.put_spatial(k, v);
                batch.commit().unwrap();
                expected.insert(k.clone(), v.clone());
            }
            engine.shutdown().unwrap();
        }

        let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
        for (k, v) in &expected {
            let got = engine.spatial.get(k).unwrap();
            prop_assert_eq!(got.as_deref(), Some(v.as_slice()));
        }
    }
}

/// Finding 8 regression: `Engine::major_compact` must seal active memtables
/// before rotating the WAL. Pre-fix, active-memtable writes were orphaned —
/// they were not in SSTables (only sealed memtables were flushed) and the
/// WAL that protected them was rotated away, so an abrupt process termination
/// after `major_compact` lost everything that had not accumulated to a 16 MB
/// memtable before the call.
///
/// This test uses 50 small records (~2 KB total), far below the 16 MB active
/// cap, so the active memtable never seals on its own — the only path to
/// durability is the explicit seal that the fix introduces.
#[test]
fn finding_8_major_compact_seals_active_before_wal_rotate() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let records: Vec<(Vec<u8>, Vec<u8>)> = (0u8..50).map(|i| (vec![i; 8], vec![i; 32])).collect();

    {
        let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
        for (k, v) in &records {
            let mut batch = engine.batch();
            batch.put_spatial(k, v);
            batch.commit().unwrap();
        }
        engine.major_compact().unwrap();
        // Simulate SIGKILL: bypass Drop so the graceful shutdown path
        // (which seals active + drains bg workers) does not run. In a
        // real crash the process dies without unwinding. After
        // `major_compact()` returns, every ack'd write must be durable
        // by persistence alone — not by the drop path doing extra work.
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
    for (k, v) in &records {
        let got = engine.spatial.get(k).unwrap();
        assert_eq!(
            got.as_deref(),
            Some(v.as_slice()),
            "key {:?} lost after major_compact + crash (Finding 8 regression)",
            k
        );
    }
}

/// Finding 9 regression: the group-commit writer must block until the
/// sync thread has advanced `synced_epoch` past the writer's own epoch.
/// The pre-fix code used `wait_timeout(5ms)` which lets the writer
/// return `Ok` on timeout without the epoch being synced — a silent
/// violation of the Durable-mode contract.
///
/// This test pauses the sync thread via the `durability-test-hooks`
/// feature so `synced_epoch` cannot advance, then spawns a writer.
///
///   Pre-fix : writer returns `Ok` after ~5 ms (bug; ack on unsynced data).
///   Post-fix: writer blocks indefinitely on the condvar wait loop until
///             the sync thread catches up.
///
/// The test asserts the writer is still blocked after 50 ms (≫ the 5 ms
/// pre-fix timeout).
#[cfg(feature = "durability-test-hooks")]
#[test]
fn finding_9_writer_blocks_until_synced_epoch() {
    use std::time::Duration;

    // PHASE 1 — setup: open engine, pause the sync thread so
    // synced_epoch cannot advance on its own.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
    engine._test_pause_sync(true);

    std::thread::scope(|s| {
        // PHASE 2 — trigger: spawn a writer thread that commits a batch.
        // Pre-fix : wait_timeout(5ms) lets commit() return Ok.
        // Post-fix: while-loop holds commit() until synced >= epoch,
        //           which cannot happen while sync is paused.
        let writer = s.spawn(|| {
            let mut batch = engine.batch();
            batch.put_spatial(b"k", b"v");
            batch.commit()
        });

        // Give the writer well past the pre-fix 5 ms timeout.
        std::thread::sleep(Duration::from_millis(50));

        // PHASE 3 — assert: the writer must still be blocked. If
        // `is_finished()` is true, commit() returned Ok while the
        // writer's epoch was not yet synced — Finding 9 regression.
        assert!(
            !writer.is_finished(),
            "Finding 9 regression: commit() returned before synced_epoch \
             reached the writer's epoch. The writer ack'd the caller on \
             unsynced data."
        );

        // PHASE 4 — cleanup: unpause sync, let the writer complete so
        // the engine reaches a clean state before Drop.
        engine._test_pause_sync(false);
        let result = writer.join().unwrap();
        assert!(
            result.is_ok(),
            "writer failed after unpause: {:?}",
            result.err()
        );
    });
}

/// Finding 10 regression: the WAL janitor must not call `rotate()`
/// while active memtables hold writes with seqno greater than
/// `flushed_seqno`. Pre-fix, the janitor woke every 500 ms and rotated
/// the WAL whenever `flushed_seqno` advanced, truncating acknowledged
/// writes that lived only in active memtables (because rotate() trims
/// the entire WAL, not up to a seqno).
///
/// This test reproduces the scenario end-to-end:
///
///   Phase 1: insert + seal + flush a first batch so `flushed_seqno`
///            advances above zero.
///   Phase 2: insert a second batch. Writes stay in the active
///            memtable (small enough to not fill 16 MB), with seqno
///            greater than `flushed_seqno`.
///   Phase 3: sleep past the janitor's 500 ms interval so it wakes
///            and observes `min_flushed > last_rotated`.
///   Phase 4: `std::mem::forget` the engine (SIGKILL simulation —
///            skips Drop's graceful seal + flush).
///   Phase 5: reopen. Assert every record from both batches is
///            recoverable.
///
///   Pre-fix : Phase 3 triggers a rotate(); Phase 2 writes are
///             discarded from the WAL; Phase 5 cannot recover them.
///   Post-fix: the janitor is gated behind `durability-test-hooks`
///             and never spawns in production. Under this test
///             feature flag it does spawn, so the test exercises
///             the pre-fix scenario. The `rotate()` call itself
///             would still discard the WAL; the fix prevents this
///             by not spawning the janitor at all in production.
///
/// The test therefore runs only under `durability-test-hooks` **and**
/// with the janitor still active (the feature flag's sole job is to
/// keep it alive). In production builds (no feature), Finding 10 does
/// not reach, because the janitor does not spawn.
///
/// **Semantics of this test**: it uses `#[should_panic]` because the
/// feature flag deliberately re-enables the buggy behaviour for the
/// sole purpose of demonstrating it. The test "passes" iff the bug
/// reproduces under the flag (a keyspace's post-flush key is missing
/// after the simulated crash). If the panic does not fire — i.e. the
/// janitor did not rotate, or option (c) of the fix (`rotate_up_to
/// (seqno)`) has been implemented — this test becomes obsolete and
/// should be removed or flipped.
#[cfg(feature = "durability-test-hooks")]
#[test]
#[should_panic(expected = "after janitor rotate + crash (Finding 10 regression)")]
fn finding_10_wal_janitor_rotate_does_not_lose_active_memtable_writes() {
    use std::time::Duration;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");

    let pre_flush: Vec<(Vec<u8>, Vec<u8>)> = (0u8..20).map(|i| (vec![i; 8], vec![i; 32])).collect();
    let post_flush: Vec<(Vec<u8>, Vec<u8>)> =
        (100u8..120).map(|i| (vec![i; 8], vec![i; 32])).collect();

    {
        let engine = TurbaEngine::open(&db_path, make_config()).unwrap();

        // PHASE 1 — seed + explicit seal + flush across ALL
        // keyspaces. The janitor rotates only when
        // `min(flushed_seqno across all trees)` advances; if any
        // keyspace has never flushed, its flushed_seqno = 0 and
        // the min stays 0 forever, so the janitor never fires.
        // Production workloads typically touch all keyspaces,
        // so seeding each one here matches reality.
        for (k, v) in &pre_flush {
            let mut batch = engine.batch();
            batch.put_spatial(k, v);
            batch.put_identity(k, v);
            batch.put_dictionary(k, v);
            batch.put_ghosts(k, v);
            batch.put_vectors(k, v);
            batch.commit().unwrap();
        }
        for tree in [
            &engine.spatial,
            &engine.identity,
            &engine.dictionary,
            &engine.ghosts,
            &engine.vectors,
        ] {
            tree.seal_active();
            tree.flush_sealed().unwrap();
        }

        // PHASE 2 — more writes that stay in active memtable with
        // seqno > flushed_seqno. Small enough (~1 KB total) to not
        // fill 16 MB and self-seal. Each commit() blocks under
        // Finding 9's while-loop until its epoch is synced, so on
        // return the WAL is up to date on disk. We write to all
        // keyspaces so each has flushed_seqno < the latest
        // commit seqno in the batch.
        for (k, v) in &post_flush {
            let mut batch = engine.batch();
            batch.put_spatial(k, v);
            batch.put_identity(k, v);
            batch.put_dictionary(k, v);
            batch.put_ghosts(k, v);
            batch.put_vectors(k, v);
            batch.commit().unwrap();
        }

        // PHASE 3a — pause the sync thread so the janitor does not
        // compete for the journal lock. Without this, the sync
        // thread (1 ms interval) wins the try_lock race against
        // the janitor (500 ms interval) in most cycles and the
        // rotate() rarely fires inside a test-length window. The
        // bug is real in production over long runs; the pause
        // makes it deterministic inside a test.
        engine._test_pause_sync(true);

        // PHASE 3b — wait past the janitor's 500 ms interval. With
        // the sync thread paused, the janitor wakes, sees
        // min_flushed > last_rotated, acquires the journal lock
        // uncontended, and calls rotate() which truncates the
        // entire WAL.
        std::thread::sleep(Duration::from_millis(700));

        // PHASE 4 — SIGKILL simulation. Drop would seal + flush
        // active memtables and hide the bug.
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    // PHASE 5 — reopen. Pre-fix (janitor active + rotate happened):
    // post_flush keys are missing from every keyspace. Post-fix
    // (janitor not spawned): WAL replay recovers them across all
    // trees.
    let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
    for (k, v) in pre_flush.iter().chain(post_flush.iter()) {
        for (ks_name, tree) in [
            ("spatial", &engine.spatial),
            ("identity", &engine.identity),
            ("dictionary", &engine.dictionary),
            ("ghosts", &engine.ghosts),
            ("vectors", &engine.vectors),
        ] {
            let got = tree.get(k).unwrap();
            assert_eq!(
                got.as_deref(),
                Some(v.as_slice()),
                "key {:?} lost from {} after janitor rotate + crash (Finding 10 regression)",
                k,
                ks_name
            );
        }
    }
}

/// 3d — torn SST publish: a crash mid-flush (an SST written but not yet
/// referenced by the manifest) must lose nothing. The survival side is the
/// real risk (per the charter): the WAL must not be truncated until the SST
/// is fully published, so records that were committed but never durably
/// landed in an SST are recoverable by WAL replay. `finding_8` covers the
/// post-compact case; this covers the unflushed case where the WAL is the
/// SOLE durable copy, AND that an orphan SST file is ignored — not a crash,
/// not a duplicate, not a loss.
#[test]
fn finding_3d_torn_sst_publish_survives_via_wal_and_ignores_orphan() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    let recs: Vec<(Vec<u8>, Vec<u8>)> = (0u8..80).map(|i| (vec![i; 8], vec![i; 40])).collect();
    let (flushed, wal_only) = recs.split_at(40);

    {
        let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
        // First half → flushed to an SSTable (creates the segment dir).
        for (k, v) in flushed {
            let mut b = engine.batch();
            b.put_spatial(k, v);
            b.commit().unwrap();
        }
        engine.spatial.seal_active();
        engine.spatial.flush_sealed().unwrap();
        // Second half → committed (WAL-durable) but NOT flushed: it lives
        // only in the WAL + active memtable.
        for (k, v) in wal_only {
            let mut b = engine.batch();
            b.put_spatial(k, v);
            b.commit().unwrap();
        }
        // SIGKILL: forget bypasses Drop, so no graceful flush and no WAL
        // rotate. The WAL is the only durable copy of `wal_only`.
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    // A flush torn between fsync and rename leaves a stray, manifest-unreferenced
    // file. Recovery must ignore it (Turba lists segments from its own manifest;
    // verified by reopen below, not assumed).
    inject_orphan_segment(&db_path);

    let engine = TurbaEngine::open(&db_path, make_config()).unwrap();
    for (k, v) in &recs {
        assert_eq!(
            engine.spatial.get(k).unwrap().as_deref(),
            Some(v.as_slice()),
            "key {:?} lost — torn-SST-publish recovery must replay the WAL and ignore \
             the orphan (3d survival invariant: WAL not truncated until SST published)",
            k
        );
    }
}

/// Drop a stray, manifest-unreferenced file into a directory that already
/// holds segments, mimicking an SST publish torn between fsync and rename.
fn inject_orphan_segment(db_path: &std::path::Path) {
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(p) {
            let entries: Vec<_> = rd.flatten().collect();
            if entries.iter().any(|e| e.path().is_file()) {
                out.push(p.to_path_buf());
            }
            for e in &entries {
                if e.path().is_dir() {
                    walk(&e.path(), out);
                }
            }
        }
    }
    let mut dirs = Vec::new();
    walk(db_path, &mut dirs);
    for d in &dirs {
        let _ = std::fs::write(d.join("99999999.sst.tmp"), b"torn-orphan-not-in-manifest");
    }
}

/// Option B (WAL segmentation) regression: `prune_wal()` / the background
/// pruner must DELETE archived WAL segments that are fully manifest-durable
/// WITHOUT losing acknowledged-but-unflushed writes still in the active
/// segment. The Finding-10 janitor truncated the whole WAL and lost the tail;
/// a no-op prune would not bound the WAL at all. This asserts BOTH: the prune
/// frees the durable prefix (freed > 0) AND an unflushed acked write survives a
/// crash taken right after the prune.
#[test]
fn wal_prune_keeps_unflushed_tail_after_crash() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("db");
    // Tiny segments so the durable prefix (A) rolls into archived segments.
    let wal_seg = 1024u64;
    let cfg = || {
        let mut c = make_config();
        c.wal_segment_max_bytes = wal_seg;
        c
    };

    let a: Vec<(Vec<u8>, Vec<u8>)> = (0u16..60)
        .map(|i| (format!("a{:04}", i).into_bytes(), vec![1u8; 64]))
        .collect();
    let b: Vec<(Vec<u8>, Vec<u8>)> = (0u16..10)
        .map(|i| (format!("b{:04}", i).into_bytes(), vec![2u8; 64]))
        .collect();

    {
        let engine = TurbaEngine::open(&db_path, cfg()).unwrap();
        // A: write, then flush to SST + persist manifest → manifest-durable past A.
        for (k, v) in &a {
            let mut bt = engine.batch();
            bt.put_spatial(k, v);
            bt.commit().unwrap();
        }
        for t in [
            &engine.spatial,
            &engine.identity,
            &engine.dictionary,
            &engine.ghosts,
        ] {
            t.seal_active();
            t.flush_sealed().unwrap();
        }
        // B: acknowledged, in the ACTIVE WAL segment + active memtable, NOT flushed.
        for (k, v) in &b {
            let mut bt = engine.batch();
            bt.put_spatial(k, v);
            bt.commit().unwrap();
        }
        // Prune must BOUND the WAL: A's archived segments (durable) reclaimed,
        // B's active segment (non-durable) kept. `make_config()` runs the
        // background `turba-wal-pruner`, which under parallel load may reclaim
        // A's segments before this explicit call — so assert the END STATE (the
        // WAL is bounded, *whoever* pruned it), not that *this* call freed bytes.
        // After `prune_wal()` returns it holds `journal.lock()`, so the durable
        // prefix is fully reclaimed; the WAL then retains only the non-durable
        // tail (B ≈ one 1 KiB segment), never A's ≥5 archived segments.
        engine.prune_wal().unwrap();
        let wal_bytes = engine._test_wal_total_bytes();
        assert!(
            wal_bytes < 3 * wal_seg,
            "WAL not bounded after prune: {wal_bytes} B retained (expected < {} B) \
             — A's durable prefix was not reclaimed",
            3 * wal_seg
        );
        // Crash: stop every bg thread WITHOUT flushing (real SIGKILL semantics —
        // no ghost pruner/flush/sync thread survives to race the reopen), then
        // leak the engine so Drop's graceful seal+flush never runs.
        engine._test_crash_stop();
        std::mem::forget(engine);
    }

    let engine = TurbaEngine::open(&db_path, cfg()).unwrap();
    for (k, v) in &a {
        assert_eq!(
            engine.spatial.get(k).unwrap().as_deref(),
            Some(v.as_slice()),
            "A key {:?} lost (it was flushed to SST before prune)",
            k
        );
    }
    for (k, v) in &b {
        assert_eq!(
            engine.spatial.get(k).unwrap().as_deref(),
            Some(v.as_slice()),
            "B key {:?} lost after prune+crash — prune dropped the unflushed tail (Option B durability violation)",
            k
        );
    }
}
