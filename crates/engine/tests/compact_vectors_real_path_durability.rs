//! Ticket 2 (compact-skips-vectors) — real-path regression guard.
//!
//! `cf28742` fixed COMPACT to seal + flush the `vectors` keyspace before it
//! truncates the WAL, closing a durability hole (acked vectors stranded in the
//! memtable were lost on a post-COMPACT crash). Its regression test
//! (`crates/turba-engine/tests/compact_vectors_durability.rs`) exercises the turba
//! primitive `bt.put_vectors` and replicates execute_compact's sequence by hand.
//!
//! This test closes the remaining gap: it drives the **real engine path** — the
//! `PUT` verb with a declared `VECTOR` column (V5 hoist), and the **real `COMPACT`
//! verb** (`engine.run("COMPACT")` → `execute_compact`) — over the product form
//! (`GRAVITY BY` + vector). Dataset stays below the auto-flush threshold so the
//! `vectors` memtable never auto-flushes (disk_sst=0) before COMPACT; the fix is
//! what must move it to an SSTable so the subsequent WAL truncation is safe.
//!
//! Invariant asserted: after COMPACT, every WAL-backed keyspace's acked data is
//! recoverable — i.e. `vectors` is on disk (disk_sst>0) OR still in the WAL. A
//! regression (COMPACT skipping vectors again) leaves disk_sst=0 AND WAL=0 and
//! trips the assert. Both write shapes (individual `PUT` and `PUT BATCH`) covered.

// SPDX-License-Identifier: BUSL-1.1
use std::path::Path;
use xyzdb_engine::engine::{Engine, QueryResult};

/// 1024-d vector literal, deterministic and distinct per record index.
fn vec_literal(i: usize) -> String {
    let mut s = String::with_capacity(1024 * 8);
    s.push('[');
    for j in 0..1024 {
        if j > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("{:.5}", (i as f32) * 0.01 + (j as f32) * 0.001));
    }
    s.push(']');
    s
}

fn record_count(r: &QueryResult) -> usize {
    match r {
        QueryResult::Records(v) => v.len(),
        QueryResult::PaginatedRecords { records, .. } => records.len(),
        other => panic!("expected records, got {other:?}"),
    }
}

fn schema(engine: &Engine) {
    engine.run(r#"LOBE "mem""#).expect("lobe");
    engine.run(r#"VECTOR emb IN "mem""#).expect("vector");
    engine
        .run(r#"GRAVITY BY bucket IN "mem""#)
        .expect("gravity");
}

fn nearest3(engine: &Engine) -> usize {
    let q = format!(
        r#"SCAN "mem" WHERE bucket = "b0" LIMIT 10000 | NEAREST(emb, {}, 3, cosine)"#,
        vec_literal(0)
    );
    record_count(&engine.run(&q).expect("nearest"))
}

fn wal_bytes(dir: &Path) -> u64 {
    let mut tot = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("journal") && name.ends_with(".wal") {
                if let Ok(m) = e.metadata() {
                    tot += m.len();
                }
            }
        }
    }
    tot
}

/// After a real `COMPACT`, the `vectors` acked tail must be recoverable: on disk
/// (disk_sst>0) or still in the WAL. disk_sst==0 AND WAL==0 ⇒ acked vectors are
/// stranded in the memtable and would be lost on the next crash — the Ticket 2 bug.
fn assert_vectors_durable_after_compact(engine: &Engine, dir: &Path, case: &str) {
    let v = &engine.turba().vectors;
    let disk_sst = v.disk_sst_count();
    let wal = wal_bytes(dir);
    assert!(
        disk_sst > 0 || wal > 0,
        "[{case}] Ticket 2 regression: after COMPACT vectors disk_sst=0 AND WAL=0 \
         (mem_active_bytes={}) → acked vectors stranded in RAM, lost on crash",
        v.active_memtable_size(),
    );
}

#[test]
fn compact_verb_flushes_vectors_individual_puts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open"); // Durable by default (mod.rs:60)
    schema(&engine);
    for i in 0..100 {
        let stmt = format!(
            r#"PUT {{*bucket: "b0", id: "g{i}", emb: {}}} IN "mem""#,
            vec_literal(i)
        );
        engine.run(&stmt).expect("put");
    }
    assert_eq!(nearest3(&engine), 3, "vectors present pre-compact");
    assert_eq!(
        engine.turba().vectors.disk_sst_count(),
        0,
        "precondition: not auto-flushed yet"
    );

    engine.run("COMPACT").expect("compact");
    assert_vectors_durable_after_compact(&engine, dir.path(), "individual PUT");
    assert_eq!(
        nearest3(&engine),
        3,
        "vectors still searchable after compact"
    );
}

/// Site-2 guard (defense in depth): `TurbaEngine::major_compact` now routes its
/// WAL rotate through the GUARDED `rotate_journal` (was an unguarded
/// `journal.rotate()`). Exercised via the real hoist path: after the engine-level
/// major_compact, `vectors` is flushed (disk_sst>0) and the WAL is rotated (safe
/// because the guard's watermark is MAX — everything durable).
#[test]
fn engine_major_compact_flushes_vectors_and_rotates_safely() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    schema(&engine);
    for i in 0..100 {
        let stmt = format!(
            r#"PUT {{*bucket: "b0", id: "g{i}", emb: {}}} IN "mem""#,
            vec_literal(i)
        );
        engine.run(&stmt).expect("put");
    }
    assert_eq!(
        engine.turba().vectors.disk_sst_count(),
        0,
        "precondition: not auto-flushed yet"
    );

    // The site-2 path: engine-level major_compact → seal+major all + guarded rotate.
    engine
        .turba()
        .major_compact()
        .expect("engine major_compact");

    assert!(
        engine.turba().vectors.disk_sst_count() > 0,
        "vectors flushed by engine major_compact"
    );
    assert_eq!(
        wal_bytes(dir.path()),
        0,
        "WAL rotated (safe: watermark MAX, all durable)"
    );
    assert_eq!(nearest3(&engine), 3, "vectors still searchable");
}

#[test]
fn compact_verb_flushes_vectors_put_batch() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).expect("open");
    schema(&engine);
    let mut b = String::from(r#"PUT BATCH IN "mem" ["#);
    for i in 0..100 {
        if i > 0 {
            b.push_str(", ");
        }
        b.push_str(&format!(
            r#"{{*bucket: "b0", id: "g{i}", emb: {}}}"#,
            vec_literal(i)
        ));
    }
    b.push(']');
    engine.run(&b).expect("put batch");
    assert_eq!(nearest3(&engine), 3, "vectors present pre-compact");
    assert_eq!(
        engine.turba().vectors.disk_sst_count(),
        0,
        "precondition: not auto-flushed yet"
    );

    engine.run("COMPACT").expect("compact");
    assert_vectors_durable_after_compact(&engine, dir.path(), "PUT BATCH");
    assert_eq!(
        nearest3(&engine),
        3,
        "vectors still searchable after compact"
    );
}
