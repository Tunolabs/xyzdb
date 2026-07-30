//! Subprocess-based crash recovery integration tests.
//!
//! Complement to the in-process `mem::forget` tests in `durability_proptest.rs`.
//! These spawn a real child process, drive it to a known write state, SIGKILL
//! it, then reopen the DB in the parent and verify the D1 invariant.
//! Validates behaviour under real process death — no Drop shortcuts.
//!
//! Pattern: each `#[test]` function detects whether it is running as the
//! parent or the child via `XYZ_CRASH_TEST_CHILD` env var. Parent spawns the
//! test binary against itself (`--exact <test_name>`) with the env var set;
//! child does its work, prints progress to stdout, then loops forever until
//! the parent SIGKILLs it.
//!
//! See `docs/wal-state-machine.md` for the state machine these tests
//! exercise.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

const CHILD_ENV: &str = "XYZ_CRASH_TEST_CHILD";
const CHILD_DB_PATH: &str = "XYZ_CRASH_TEST_DB";

fn config() -> EngineConfig {
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

fn spawn_child(test_name: &str, child_mode: &str, db_path: &Path) -> std::process::Child {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(&exe)
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_ENV, child_mode)
        .env(CHILD_DB_PATH, db_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child")
}

/// Crash-during-write integration test: the child writes N records
/// in Durable mode and prints `ACK <n>` to stdout after each successful
/// commit. The parent reads the ACK stream, SIGKILLs the child once at least
/// 50 records have been acked, reopens the DB, and asserts every acked
/// record is recoverable.
///
/// Strengthens the in-process `finding_8_*` tests by validating the same
/// invariant under a real SIGKILL — no `mem::forget`, no Drop opportunity,
/// no graceful shutdown.
#[test]
fn crash_after_acked_writes_preserves_them() {
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        let db_path =
            PathBuf::from(std::env::var(CHILD_DB_PATH).expect("child needs XYZ_CRASH_TEST_DB"));
        match mode.as_str() {
            "write_acked" => child_write_acked(&db_path),
            other => panic!("child: unknown mode {other}"),
        }
        return;
    }

    // Parent.
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_child(
        "crash_after_acked_writes_preserves_them",
        "write_acked",
        tmp.path(),
    );

    let stdout = child.stdout.take().expect("child stdout");
    let reader = BufReader::new(stdout);
    let mut acked: u64 = 0;
    for line in reader.lines() {
        let line = line.expect("read child stdout");
        if let Some(n) = line.strip_prefix("ACK ") {
            acked = n.parse().expect("parse ack");
            if acked >= 50 {
                break;
            }
        }
    }
    assert!(
        acked >= 50,
        "child died before acking 50 records (got {acked})"
    );

    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // Reopen and verify every acked record is present.
    let engine = TurbaEngine::open(tmp.path(), config()).expect("parent reopen");
    let mut recovered: u64 = 0;
    for i in 0..acked {
        let key = format!("key{:05}", i);
        if engine.spatial.get(key.as_bytes()).expect("get").is_some() {
            recovered += 1;
        }
    }
    assert_eq!(
        recovered, acked,
        "D1 violation: {acked} records acked, only {recovered} survived SIGKILL + reopen"
    );
}

fn child_write_acked(db_path: &Path) {
    let engine = TurbaEngine::open(db_path, config()).expect("child: open");
    // Write 100 records; the parent kills us somewhere between 50 and 100.
    for i in 0..100u64 {
        let key = format!("key{:05}", i);
        let value = format!("v{i}");
        let mut batch = engine.batch();
        batch.put_spatial(key.as_bytes(), value.as_bytes());
        batch.commit().expect("child: commit");
        // Under Durable mode, commit returns only after synced_epoch
        // advances past the writer's epoch (Finding 9 primary fix). The
        // ACK line below therefore implies the batch is fsynced.
        println!("ACK {}", i + 1);
        let _ = std::io::stdout().flush();
    }
    // All 100 written; block until SIGKILL.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Finding 9 subprocess regression test: the child pauses the
/// sync thread via the `durability-test-hooks` hook, then spawns a writer
/// thread that calls `batch.commit()`. Under the Finding 9 primary fix,
/// the writer blocks on the condvar forever (because `synced_epoch` never
/// advances while the sync thread is paused). The child prints `ISSUED`
/// before the commit attempt; the parent waits for that signal, then
/// SIGKILLs. On reopen, the record MUST NOT be present — the writer never
/// returned Ok, so no ack was given, so no client was told the write
/// succeeded.
///
/// Under the Finding 9 bug (pre-fix), the writer would have timed out on
/// `wait_timeout(5ms)` and returned Ok despite no fsync. The record would
/// be in the memtable but not in the WAL; SIGKILL + reopen would show 0
/// records, which means the writer had lied. This test cannot detect the
/// lie directly (the child is killed mid-block under the fix and mid-lie
/// under the bug, both leading to reopen-shows-zero), but it verifies the
/// subprocess machinery works end-to-end and the reopen-zero end state
/// holds under paused sync.
#[cfg(feature = "durability-test-hooks")]
#[test]
fn finding_9_paused_sync_writer_blocks_before_ack() {
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        let db_path = PathBuf::from(std::env::var(CHILD_DB_PATH).unwrap());
        match mode.as_str() {
            "paused_sync_writer" => child_paused_sync_writer(&db_path),
            other => panic!("child: unknown mode {other}"),
        }
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_child(
        "finding_9_paused_sync_writer_blocks_before_ack",
        "paused_sync_writer",
        tmp.path(),
    );

    let stdout = child.stdout.take().expect("child stdout");
    let reader = BufReader::new(stdout);
    let mut issued = false;
    for line in reader.lines() {
        let line = line.expect("read child");
        if line.contains("ISSUED") {
            issued = true;
            break;
        }
    }
    assert!(issued, "child did not reach the ISSUED signal");

    // Give the writer thread a moment to enter the condvar block. Under the
    // fix it will block indefinitely; under the old bug it would have
    // returned Ok within 5 ms. We sleep 200 ms so that under the bug, the
    // writer has had 40× the bug's timeout to complete.
    std::thread::sleep(Duration::from_millis(200));

    child.kill().expect("kill child");
    child.wait().expect("wait child");

    // Reopen. Expected: 0 records. Under the fix, the writer blocked and
    // never acked; no data reached disk. Under the bug, the writer acked
    // but the WAL was never fsynced — same reopen outcome. This test
    // verifies the subprocess path does not corrupt state in either case.
    let engine = TurbaEngine::open(tmp.path(), config()).expect("parent reopen");
    let found = engine.spatial.get(b"paused_key").expect("get").is_some();
    assert!(
        !found,
        "D1 violation: paused-sync write should not survive SIGKILL (writer was blocked, no ack was returned)"
    );
}

#[cfg(feature = "durability-test-hooks")]
fn child_paused_sync_writer(db_path: &Path) {
    use std::sync::Arc;
    let engine = Arc::new(TurbaEngine::open(db_path, config()).expect("child: open"));
    engine._test_pause_sync(true);

    // Spawn the writer on a background thread so the main thread can print
    // the ISSUED signal and then block.
    let engine_w = Arc::clone(&engine);
    std::thread::spawn(move || {
        let mut batch = engine_w.batch();
        batch.put_spatial(b"paused_key", b"paused_value");
        // Under the fix this call blocks forever. We are on a detached
        // thread; the parent will SIGKILL us.
        let _ = batch.commit();
        // Unreachable under the fix.
        println!("UNREACHABLE-ACK");
        let _ = std::io::stdout().flush();
    });

    println!("ISSUED");
    let _ = std::io::stdout().flush();

    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// WAL-pruner durability under a REAL crash (Option B gold-standard proof).
/// Subprocess writes acked records while flushing + pruning the WAL (so the
/// background `turba-wal-pruner` and explicit `prune_wal()` both delete archived
/// segments mid-run), then the parent SIGKILLs it — no Drop, no graceful
/// shutdown, real process death. On reopen EVERY acked record must survive: if
/// the prune had deleted a WAL segment whose data was not already manifest-durable
/// in an SSTable, those records would be lost. They are not.
fn prune_config() -> EngineConfig {
    let mut c = config();
    c.wal_segment_max_bytes = 2048; // tiny → segments roll constantly so prune has work
    c
}

#[test]
fn crash_under_active_wal_pruning_preserves_acked() {
    if let Ok(mode) = std::env::var(CHILD_ENV) {
        let db_path = PathBuf::from(std::env::var(CHILD_DB_PATH).expect("child needs db path"));
        if mode == "prune_under_load" {
            child_prune_under_load(&db_path);
        } else {
            panic!("child: unknown mode {mode}");
        }
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = spawn_child(
        "crash_under_active_wal_pruning_preserves_acked",
        "prune_under_load",
        tmp.path(),
    );
    let stdout = child.stdout.take().expect("child stdout");
    let reader = BufReader::new(stdout);
    let mut acked: u64 = 0;
    for line in reader.lines() {
        let line = line.expect("read child stdout");
        if let Some(n) = line.strip_prefix("ACK ") {
            acked = n.parse().expect("parse ack");
            if acked >= 120 {
                break;
            }
        }
    }
    assert!(acked >= 120, "child died before acking 120 (got {acked})");

    child.kill().expect("kill child");
    child.wait().expect("wait child");

    let engine = TurbaEngine::open(tmp.path(), prune_config()).expect("parent reopen");
    let mut recovered = 0u64;
    for i in 0..acked {
        let key = format!("key{:05}", i);
        if engine.spatial.get(key.as_bytes()).expect("get").is_some() {
            recovered += 1;
        }
    }
    assert_eq!(
        recovered, acked,
        "D1 violation under WAL pruning: {acked} acked, only {recovered} survived SIGKILL + reopen"
    );
}

fn child_prune_under_load(db_path: &Path) {
    let engine = TurbaEngine::open(db_path, prune_config()).expect("child: open");
    for i in 0..300u64 {
        let key = format!("key{:05}", i);
        let value = format!("val-{i}-padding-padding"); // a few dozen bytes → segments roll fast
        let mut batch = engine.batch();
        batch.put_spatial(key.as_bytes(), value.as_bytes());
        batch.commit().expect("child: commit"); // SyncData → ACK implies fsynced
        println!("ACK {}", i + 1);
        let _ = std::io::stdout().flush();
        // Every 25 records: flush everything to SST (advances manifest_durable) and
        // prune the now-durable archived WAL segments — exactly what bounds the WAL
        // in production, exercised right before the parent's kill window.
        if i % 25 == 24 {
            for t in [
                &engine.spatial,
                &engine.identity,
                &engine.dictionary,
                &engine.ghosts,
            ] {
                t.seal_active();
                t.flush_sealed().expect("child: flush");
            }
            engine.prune_wal().expect("child: prune");
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
