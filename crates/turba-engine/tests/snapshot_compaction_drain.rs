//! H12 regression: `create_snapshot` must DRAIN in-flight compaction.
//!
//! The bug: `create_snapshot` set `compaction_enabled = false` (which only
//! stops *new* background passes) and then hard-linked the live SSTs without
//! waiting for a pass already past that gate. That pass' `delete_compacted_inputs`
//! could unlink an SST between the snapshot's `live_table_paths()` capture and
//! its `hard_link()` (→ ENOENT), or persist a MANIFEST skewed against the linked
//! SST set. The soak hit this 12/13 times.
//!
//! The fix acquires each tree's `compaction_lock` across the capture window, so
//! `create_snapshot` blocks until the in-flight pass finishes and no compaction
//! can delete an input mid-capture.
//!
//! This test makes the race deterministic with a `CompactionObserver` that
//! parks the compaction mid-flight (holding `compaction_lock`), then proves
//! `create_snapshot` cannot complete until the compaction is released — i.e.
//! it drains. Without the fix the snapshot returns immediately while the
//! compaction is still parked, which the timed assertion catches.

// SPDX-License-Identifier: BUSL-1.1
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use turba_engine::compaction::worker::CompactionObserver;
use turba_engine::config::EngineConfig;
use turba_engine::engine::TurbaEngine;
use turba_engine::snapshot;

const KEYS: u64 = 1_000;

/// Observer that parks the compaction on its first observed entry, signalling
/// the main thread it is in-flight, then blocks until explicitly released.
/// `compaction_lock` is held for the whole `major_compact_with_observer` call,
/// so while parked no other compaction — and, with the fix, no snapshot —
/// can proceed past the lock.
struct PauseObserver {
    /// Flipped on the first `observe`; signalled via `entered`.
    fired: AtomicBool,
    entered: (Mutex<bool>, Condvar),
    release: (Mutex<bool>, Condvar),
}

impl PauseObserver {
    fn new() -> Self {
        Self {
            fired: AtomicBool::new(false),
            entered: (Mutex::new(false), Condvar::new()),
            release: (Mutex::new(false), Condvar::new()),
        }
    }

    fn wait_until_in_flight(&self) {
        let (m, c) = &self.entered;
        let mut g = m.lock().unwrap();
        while !*g {
            g = c.wait(g).unwrap();
        }
    }

    fn release(&self) {
        let (m, c) = &self.release;
        *m.lock().unwrap() = true;
        c.notify_all();
    }
}

impl CompactionObserver for PauseObserver {
    fn observe(&self, _key: &[u8], _value: &[u8]) {
        // Only the first entry parks; the rest stream through normally.
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let (m, c) = &self.entered;
            *m.lock().unwrap() = true;
            c.notify_all();
        }
        let (m, c) = &self.release;
        let mut g = m.lock().unwrap();
        while !*g {
            g = c.wait(g).unwrap();
        }
    }
}

fn open(dir: &std::path::Path) -> TurbaEngine {
    TurbaEngine::open(dir, EngineConfig::default()).expect("engine open")
}

/// Write `KEYS` keys with `tag` in the value, then seal + flush so the
/// generation lands as its own L0 SSTable (`persist()` only syncs the WAL —
/// it does not flush the memtable, so two `persist`s would leave zero SSTs
/// and `major_compact` nothing to merge, never invoking the observer).
fn write_gen(engine: &TurbaEngine, tag: &str) {
    let mut batch = engine.batch();
    for i in 0..KEYS {
        let k = format!("k-{i:08}");
        let v = format!("{tag}-{i}");
        batch.put_spatial(k.as_bytes(), v.as_bytes());
    }
    batch.commit().expect("commit");
    engine.spatial.seal_active();
    engine.spatial.flush_sealed().expect("flush");
}

#[test]
fn create_snapshot_drains_in_flight_compaction() {
    let src = tempfile::tempdir().unwrap();
    let engine = open(src.path());

    // Two overlapping L0 SSTs (same key range, newer values win) force a
    // real merge — not a trivial move — so the observer is invoked.
    write_gen(&engine, "v1");
    write_gen(&engine, "v2");

    let obs = Arc::new(PauseObserver::new());
    let snap_done = Arc::new(AtomicBool::new(false));
    let engine_ref = &engine;

    std::thread::scope(|scope| {
        // Thread T: start a major compaction that parks mid-flight while
        // holding `compaction_lock`.
        let obs_t = Arc::clone(&obs);
        let compactor = scope.spawn(move || {
            engine_ref
                .spatial
                .major_compact_with_observer(Some(obs_t.as_ref()))
        });

        // Wait until the compaction is genuinely in-flight (lock held).
        obs.wait_until_in_flight();

        // Thread S: take a snapshot. With the fix it blocks on the
        // compaction lock until T releases; without it, it returns now.
        let snap_done_s = Arc::clone(&snap_done);
        let snapshotter = scope.spawn(move || {
            let meta = engine_ref.create_snapshot("drain-test");
            snap_done_s.store(true, Ordering::SeqCst);
            meta
        });

        // The compaction is still parked. A correctly-draining snapshot
        // CANNOT have finished. (Without the drain it completes here.)
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !snap_done.load(Ordering::SeqCst),
            "create_snapshot completed while a compaction was in-flight — it did \
             not drain (H12 regression)"
        );

        // Release the compaction; the snapshot can now proceed.
        obs.release();

        compactor.join().unwrap().expect("major compaction");
        let meta = snapshotter.join().unwrap().expect("snapshot");
        assert_eq!(meta.name, "drain-test");
    });

    assert!(snap_done.load(Ordering::SeqCst));

    // The snapshot must restore cleanly with every key at its newest value.
    drop(engine);
    let snap_dir = src.path().join("snapshots").join("drain-test");
    let target = tempfile::tempdir().unwrap();
    let target_path = target.path().join("restored");
    snapshot::restore_snapshot(&snap_dir, &target_path).expect("restore");

    let restored = open(&target_path);
    for i in 0..KEYS {
        let k = format!("k-{i:08}");
        let v = restored.spatial.get(k.as_bytes()).expect("get");
        let expected = format!("v2-{i}");
        assert_eq!(
            v.as_deref(),
            Some(expected.as_bytes()),
            "key {k} missing or stale after restore"
        );
    }
}

/// The drain must happen BEFORE the WAL lock, so a long in-flight compaction
/// does NOT inflate the writer-blocking window (`lock_window_us`). A v0.8.1 soak
/// saw a 40 s window when a snapshot landed during a post-bulk major compaction:
/// the drain was inside the WAL-lock window and stalled every writer for its
/// full duration. Here we park a compaction for 800 ms; the recorded lock window
/// must stay small (the drain is excluded). With the drain inside the window
/// this assertion fails (window ≈ 800 ms+).
#[test]
fn lock_window_excludes_compaction_drain() {
    let src = tempfile::tempdir().unwrap();
    let engine = open(src.path());

    write_gen(&engine, "v1");
    write_gen(&engine, "v2");

    let obs = Arc::new(PauseObserver::new());
    let engine_ref = &engine;

    let window_us = std::thread::scope(|scope| {
        let obs_t = Arc::clone(&obs);
        let compactor = scope.spawn(move || {
            engine_ref
                .spatial
                .major_compact_with_observer(Some(obs_t.as_ref()))
        });

        obs.wait_until_in_flight();

        // create_snapshot must drain this parked compaction. With the drain
        // before the WAL lock, the snapshotter blocks here with writers
        // UNBLOCKED — the wait is not part of lock_window_us.
        let snapshotter = scope.spawn(move || engine_ref.create_snapshot("win"));

        // Hold the compaction far longer than any real seal+fsync+link window.
        std::thread::sleep(Duration::from_millis(800));
        obs.release();

        compactor.join().unwrap().expect("major compaction");
        let meta = snapshotter.join().unwrap().expect("snapshot");
        meta.lock_window_us
    });

    assert!(
        window_us < 200_000,
        "writer-blocking window must exclude the ~800 ms compaction drain; got {window_us} us"
    );
}
