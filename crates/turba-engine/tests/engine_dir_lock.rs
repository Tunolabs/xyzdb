//! C7 — data-dir single-writer lock + clean reopen.
//!
//! The `--embed` deployment runs one engine per data dir. Without an exclusive
//! lock, a stray second opener (a leftover `xyzdb-mcp --embed` plus a fresh one
//! on the same dir) would give two writers one LSM → silent corruption. These
//! tests pin: (1) a concurrent open of the same dir is rejected; (2) the lock
//! releases on drop and a reopen recovers the data (survives a "restart").

use turba_engine::config::EngineConfig;
use turba_engine::engine::TurbaEngine;

fn open(dir: &std::path::Path) -> turba_engine::error::Result<TurbaEngine> {
    TurbaEngine::open(dir, EngineConfig::default())
}

#[test]
fn concurrent_open_same_dir_is_rejected() {
    let d = tempfile::tempdir().unwrap();

    let a = open(d.path()).expect("first open holds the lock");

    // Second opener of the SAME dir must fail fast, not corrupt the store.
    let b = open(d.path());
    assert!(
        b.is_err(),
        "a second open of the same data dir must be rejected while the first is alive"
    );

    // A different dir is unaffected (the lock is per-dir).
    let other = tempfile::tempdir().unwrap();
    let o = open(other.path()).expect("a different dir opens fine");
    drop(o);

    // Releasing the first lock lets a later opener succeed.
    drop(a);
    let c = open(d.path()).expect("open succeeds once the prior lock is dropped");
    drop(c);
}

#[test]
fn reopen_after_drop_recovers() {
    let d = tempfile::tempdir().unwrap();
    {
        let e = open(d.path()).expect("open");
        let mut batch = e.batch();
        batch.put_spatial(b"k1", b"v1");
        batch.commit().expect("commit");
        e.persist().expect("persist");
    } // drop → lock released, fds closed

    // Reopen the same dir (simulates a process restart): lock re-acquired,
    // data recovered from the WAL/SSTs.
    let e2 = open(d.path()).expect("reopen after the previous engine dropped");
    assert_eq!(
        e2.spatial.get(b"k1").expect("get").as_deref(),
        Some(&b"v1"[..]),
        "data must survive a restart (WAL recovery on reopen)"
    );
}
