//! SCRUB — proactive on-disk integrity verification (alert, not repair).
//!
//! Every SST data block carries an XXH3-128 checksum (turba block.rs). SCRUB
//! walks the live SSTs of every keyspace and re-verifies each block's checksum
//! straight from disk (plus each MANIFEST), surfacing silent bit-rot before a
//! query hits it. This repro TRIGGERS the fault: flip a byte inside a data
//! block on disk, then SCRUB must report it — and report clean otherwise.

use std::path::{Path, PathBuf};
use xyzdb_engine::engine::{Engine, QueryResult};

fn run(engine: &Engine, s: &str) -> QueryResult {
    engine.run(s).unwrap_or_else(|e| panic!("{s:?}: {e:?}"))
}

fn msg(r: QueryResult) -> String {
    match r {
        QueryResult::Ok { message, .. } => message,
        other => panic!("expected Ok message, got {other:?}"),
    }
}

/// First `*.sst` file found under `dir` (recursive).
fn find_sst(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(f) = find_sst(&p) {
                return Some(f);
            }
        } else if p.extension().is_some_and(|e| e == "sst") {
            return Some(p);
        }
    }
    None
}

#[test]
fn scrub_reports_clean_then_detects_block_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "items""#);
    // Enough records to fill a data block, then flush to SST(s).
    for i in 0..200 {
        run(
            &engine,
            &format!(r#"PUT {{id: {i}, data: "padding-bytes-to-grow-the-block-{i}"}} IN "items""#),
        );
    }
    run(&engine, "COMPACT");

    // A healthy database scrubs clean.
    let clean = msg(run(&engine, "SCRUB"));
    assert!(
        clean.contains("clean"),
        "a healthy database must scrub clean, got: {clean}"
    );

    // Inject bit-rot: flip a byte inside the first data block (past the 34-byte
    // block header, well before the index/footer tail).
    let sst = find_sst(dir.path()).expect("an SST file must exist after COMPACT");
    let mut bytes = std::fs::read(&sst).unwrap();
    assert!(bytes.len() > 40, "SST unexpectedly tiny");
    bytes[40] ^= 0xFF;
    std::fs::write(&sst, &bytes).unwrap();

    // SCRUB reads raw bytes from disk (bypassing the decoded block cache), so
    // it sees the corruption and reports it.
    let dirty = msg(run(&engine, "SCRUB"));
    assert!(
        dirty.contains("FOUND") && dirty.contains("block="),
        "SCRUB must report the injected block corruption, got: {dirty}"
    );
}
