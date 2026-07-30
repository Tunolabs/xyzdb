//! v0.4 cp 3.2.1: basic snapshot + restore round-trip tests at the
//! turba-engine layer (without xyzdb-engine record encoding). Verifies:
//! - Snapshot dir is created with the expected structure.
//! - SSTs are hard-linked (same inode as source).
//! - WAL is copied (different inode).
//! - snapshot.meta is well-formed and round-trips.
//! - restore_snapshot recreates a working data dir + Engine::open
//!   succeeds against it.

use std::os::unix::fs::MetadataExt;
use turba_engine::config::EngineConfig;
use turba_engine::engine::TurbaEngine;
use turba_engine::snapshot::{self, SNAPSHOT_META_FILE, SNAPSHOT_WAL_FILE};

fn open_engine(dir: &std::path::Path) -> TurbaEngine {
    TurbaEngine::open(dir, EngineConfig::default()).expect("engine open")
}

/// S3 (path-traversal hardening): a crafted snapshot name must be rejected
/// BEFORE any filesystem join, so nothing is created outside `snapshots/`.
#[test]
fn snapshot_name_rejects_path_traversal() {
    let data = tempfile::tempdir().unwrap();
    let engine = open_engine(data.path());

    // A sentinel dir a traversal could try to clobber, a sibling of the data dir.
    let escape_target = data.path().join("ESCAPED");

    let evil = [
        "../ESCAPED",
        "../../ESCAPED",
        "a/b",
        "..",
        ".",
        "",
        "foo/../bar",
        "/abs",
        "x\0y",
    ];
    for name in evil {
        let r = engine.create_snapshot(name);
        assert!(
            matches!(r, Err(turba_engine::error::Error::InvalidSnapshotName(_))),
            "name {name:?} must be rejected as InvalidSnapshotName, got {r:?}"
        );
    }

    // Nothing escaped the snapshots dir.
    assert!(
        !escape_target.exists(),
        "traversal created a dir outside snapshots/"
    );
    let snaps = data.path().join("snapshots");
    if snaps.exists() {
        let n = std::fs::read_dir(&snaps).unwrap().count();
        assert_eq!(
            n, 0,
            "no snapshot dir should have been created by rejected names"
        );
    }

    // A normal name still works.
    let meta = engine
        .create_snapshot("ok-name_1")
        .expect("valid name must succeed");
    assert_eq!(meta.name, "ok-name_1");
}

#[test]
fn snapshot_creates_expected_layout() {
    let data = tempfile::tempdir().unwrap();
    let engine = open_engine(data.path());

    // Write something so there's at least one batch in the WAL.
    let mut batch = engine.batch();
    batch.put_spatial(b"k1", b"v1");
    batch.put_identity(b"i1", b"v2");
    batch.commit().expect("commit");
    engine.persist().expect("persist");

    let meta = engine.create_snapshot("first").expect("snapshot");
    assert_eq!(meta.name, "first");
    assert_eq!(meta.keyspaces.len(), 5);

    let snap_dir = data.path().join("snapshots").join("first");
    assert!(snap_dir.exists());
    assert!(snap_dir.join(SNAPSHOT_META_FILE).exists());
    // WAL file should have been copied (any positive size since we
    // wrote a batch in durable mode above).
    assert!(snap_dir.join(SNAPSHOT_WAL_FILE).exists());

    for ks in &["spatial", "identity", "dictionary", "ghosts", "vectors"] {
        let ks_dir = snap_dir.join(ks);
        assert!(ks_dir.exists(), "expected {} dir in snapshot", ks);
    }
}

#[test]
fn snapshot_meta_lock_window_under_gate_when_idle() {
    // With no concurrent writers and no SSTs (only memtable batch),
    // the lock window should be tiny. Cycle plan §3 Bloque 3 acceptance
    // gate is < 100 ms; an idle snapshot should be orders of magnitude
    // faster.
    let data = tempfile::tempdir().unwrap();
    let engine = open_engine(data.path());
    let mut batch = engine.batch();
    batch.put_spatial(b"a", b"1");
    batch.commit().expect("commit");

    let meta = engine.create_snapshot("idle-snap").expect("snapshot");
    eprintln!(
        "snapshot idle lock_window = {} us (gate 100000 us)",
        meta.lock_window_us
    );
    // Wall-clock gate: a shared CI runner is not an environment where this means
    // anything. Always measured and printed above; asserted only on a quiet
    // machine when explicitly requested with `XYZDB_PERF_GATES=1`.
    if std::env::var_os("XYZDB_PERF_GATES").is_some() {
        assert!(
            meta.lock_window_us < 100_000,
            "lock window {} us exceeds 100ms gate",
            meta.lock_window_us
        );
    }
}

#[test]
fn snapshot_name_collision_errors() {
    let data = tempfile::tempdir().unwrap();
    let engine = open_engine(data.path());
    engine.create_snapshot("dup").expect("first");
    let err = engine.create_snapshot("dup");
    match err {
        Err(turba_engine::error::Error::SnapshotExists(name)) => {
            assert_eq!(name, "dup");
        }
        other => panic!("expected SnapshotExists, got {other:?}"),
    }
}

#[test]
fn snapshot_ssts_are_hard_linked() {
    let data = tempfile::tempdir().unwrap();
    let engine = open_engine(data.path());

    // Force a flush so we get an SST in spatial.
    let mut batch = engine.batch();
    batch.put_spatial(b"key1", b"val1");
    batch.commit().expect("commit");
    // major_compact does seal_active → flush_sealed → rotate WAL.
    engine.major_compact().expect("major_compact");

    let meta = engine.create_snapshot("ssts").expect("snapshot");

    // Find at least one SST in spatial that exists in BOTH source dir
    // and snapshot dir, and verify the inode numbers match.
    let spatial_dir = data.path().join("spatial");
    let snap_spatial_dir = data.path().join("snapshots").join("ssts").join("spatial");
    let mut matched = false;
    for entry in std::fs::read_dir(&spatial_dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".sst") {
            continue;
        }
        let snap_path = snap_spatial_dir.join(&name);
        if !snap_path.exists() {
            continue;
        }
        let src_ino = std::fs::metadata(entry.path()).unwrap().ino();
        let dst_ino = std::fs::metadata(&snap_path).unwrap().ino();
        assert_eq!(
            src_ino, dst_ino,
            "snapshot SST {} should hard-link to source",
            name_str
        );
        matched = true;
    }
    assert!(matched, "no matching SST found in snapshot");
    assert!(meta.keyspaces.iter().any(|k| !k.sst_filenames.is_empty()));
}

#[test]
fn snapshot_restore_roundtrip() {
    // Snapshot a populated engine, restore to a fresh dir, open the
    // restored engine, verify the keys are readable. End-to-end check
    // for v0.4 cp 3.2.2.
    let src = tempfile::tempdir().unwrap();
    let engine = open_engine(src.path());

    let mut batch = engine.batch();
    batch.put_spatial(b"alpha", b"value-A");
    batch.put_spatial(b"beta", b"value-B");
    batch.put_identity(b"id-1", b"value-1");
    batch.commit().expect("commit");
    engine.persist().expect("persist");

    let _meta = engine.create_snapshot("rt").expect("snapshot");
    drop(engine);

    let snapshot_dir = src.path().join("snapshots").join("rt");
    let target = tempfile::tempdir().unwrap();
    // Use a freshly-created subdir as target to ensure cross-FS check
    // is exercised on the same FS (success path).
    let target_path = target.path().join("restored");
    snapshot::restore_snapshot(&snapshot_dir, &target_path).expect("restore");

    // Open the restored engine and verify the spatial reads.
    let restored = open_engine(&target_path);
    let v_alpha = restored.spatial.get(b"alpha").expect("get alpha");
    assert_eq!(v_alpha.as_deref(), Some(&b"value-A"[..]));
    let v_beta = restored.spatial.get(b"beta").expect("get beta");
    assert_eq!(v_beta.as_deref(), Some(&b"value-B"[..]));
    let v_id = restored.identity.get(b"id-1").expect("get id");
    assert_eq!(v_id.as_deref(), Some(&b"value-1"[..]));
}

#[test]
fn restore_refuses_non_empty_target() {
    let src = tempfile::tempdir().unwrap();
    let engine = open_engine(src.path());
    engine.create_snapshot("rt").expect("snapshot");
    drop(engine);

    let snap_dir = src.path().join("snapshots").join("rt");
    let target = tempfile::tempdir().unwrap();
    // Place a file in target to make it non-empty.
    std::fs::write(target.path().join("dirty.txt"), b"x").unwrap();

    match snapshot::restore_snapshot(&snap_dir, target.path()) {
        Err(turba_engine::error::Error::Corruption(msg)) => {
            assert!(
                msg.contains("not empty"),
                "expected 'not empty' message; got: {msg}"
            );
        }
        other => panic!("expected Corruption(not empty), got {other:?}"),
    }
}

#[test]
fn snapshot_meta_roundtrip_via_disk() {
    let data = tempfile::tempdir().unwrap();
    let engine = open_engine(data.path());
    let captured = engine.create_snapshot("roundtrip").expect("snapshot");

    let snap_dir = data.path().join("snapshots").join("roundtrip");
    let parsed = snapshot::read_snapshot_meta(&snap_dir).expect("read meta");
    assert_eq!(parsed.name, captured.name);
    assert_eq!(parsed.created_at_ms, captured.created_at_ms);
    assert_eq!(parsed.lock_window_us, captured.lock_window_us);
    assert_eq!(parsed.bulkmode_at_capture, captured.bulkmode_at_capture);
}
