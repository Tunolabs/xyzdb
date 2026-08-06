//! Artifact generator: a **healthy** datadir that actually has L1+ levels.
//!
//! Purpose: the negative control for E1, the bloom-vs-data comparator (see
//! `docs-internal/analisis-bloom-falso-negativo-raiz.md`). E1's positive control
//! already exists — `bloom_false_negative_recovery.rs` forges a bloom that
//! disagrees with its data. What was missing is a control that can FAIL: a real,
//! healthy corpse over which E1 must report **zero** findings.
//!
//! Why not just use the datadir that happened to survive on this machine: it is a
//! single SST, so it has **no L1+ run at all**. "Zero findings" over a structure
//! that trivial says nothing about the gate under suspicion — `get_at`'s per-level
//! binary search only exists from L1 up, and the engine's own comment records that
//! an unsorted L1+ level once "made point reads silently miss keys at scale". A
//! control that cannot exercise the mechanism is not a control — the same
//! reasoning that made this suite serialise its counter tests.
//!
//! So this generator ASSERTS that L1+ came out as a MULTI-TABLE run — before and
//! after a clean reopen — and fails loudly otherwise. One table at a level is not
//! enough: a binary search over a single element has nothing to get wrong. It can
//! never hand back a trivial structure that merely looks like a passing control.
//!
//! Run explicitly (it is `#[ignore]`d — it writes outside the test tempdir):
//! ```text
//! XYZ_CORPSE_DIR=/tmp/healthy-corpse TURBA_TEST_MEMTABLE_BYTES=65536 \
//!   cargo test -p turba-engine --test corpse_generator -- --ignored --nocapture
//! ```
//!
//! `TURBA_TEST_MEMTABLE_BYTES` is the engine's own diagnostic hook for exercising
//! "deep LSM levels after compaction on tiny datasets instead of requiring
//! hundreds of thousands of records" (`engine.rs`). It is REQUIRED here rather
//! than set from inside the test: it is a named condition of the artifact, so it
//! belongs in the command that produced it, not hidden in the code. Without it a
//! few MB collapse into a single L1 table — see the multi-table assertion below.

// SPDX-License-Identifier: BUSL-1.1
use turba_engine::config::{EngineConfig, IoSchedulerMode, StorageProfile};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;

/// Records per keyspace. With the small-memtable hook this yields several L0
/// flushes, so the leveled compaction below produces a multi-table L1 run.
const RECORDS: usize = 20_000;
/// Records per committed batch.
const BATCH: usize = 500;

fn config() -> EngineConfig {
    EngineConfig {
        cache_size_bytes: 4 * 1024 * 1024,
        storage_profile: StorageProfile::Ssd,
        persist_mode: PersistMode::SyncData,
        wal_path: None,
        wal_segment_max_bytes: 64 * 1024 * 1024,
        io_scheduler: IoSchedulerMode::Ssd,
        l0_batch_override: None,
        block_cache_lane_admission: true,
        ..Default::default()
    }
}

/// Total tables sitting at L1 and below (i.e. excluding L0).
fn l1_plus_tables(counts: &[usize]) -> usize {
    counts.iter().skip(1).sum()
}

#[test]
#[ignore = "artifact generator: set XYZ_CORPSE_DIR + TURBA_TEST_MEMTABLE_BYTES, run with --ignored"]
fn generate_healthy_compacted_corpse() {
    let dir = std::env::var("XYZ_CORPSE_DIR").expect(
        "set XYZ_CORPSE_DIR=<path> — this generator writes a durable artifact \
         outside any tempdir, so the destination must be explicit",
    );
    assert!(
        std::env::var("TURBA_TEST_MEMTABLE_BYTES").is_ok(),
        "set TURBA_TEST_MEMTABLE_BYTES (e.g. 65536) — without small memtables the \
         dataset collapses into ONE L1 table, and a single-table level cannot \
         exercise get_at's binary search, i.e. it would be a control that cannot fail"
    );
    let path = std::path::PathBuf::from(&dir);
    assert!(
        !path.exists()
            || std::fs::read_dir(&path)
                .map(|d| d.count() == 0)
                .unwrap_or(false),
        "{dir} already exists and is not empty — refusing to overwrite an artifact \
         that may itself be evidence"
    );
    std::fs::create_dir_all(&path).expect("create corpse dir");

    // ── Write ────────────────────────────────────────────────────────────────
    // Both `spatial` and `dictionary` get data: `dictionary` is the keyspace that
    // made this investigation urgent (a missed point-get there lets a duplicate
    // anchor through), so a corpse without it could not exercise the case that
    // matters.
    {
        let engine = TurbaEngine::open(&path, config()).expect("open");
        for chunk_start in (0..RECORDS).step_by(BATCH) {
            let mut batch = engine.batch();
            for i in chunk_start..(chunk_start + BATCH).min(RECORDS) {
                let k = format!("key{i:08}");
                let v = format!("value-{i}-{}", "p".repeat(64));
                batch.put_spatial(k.as_bytes(), v.as_bytes());
                batch.put_dictionary(format!("anchor{i:08}").as_bytes(), k.as_bytes());
            }
            batch.commit().expect("commit");
        }

        // Drive the NATURAL leveled compaction, not `major_compact`: a major
        // compaction merges everything into ONE output table, and a level holding a
        // single table cannot exhibit an ordering/overlap problem — the very thing
        // the positional gate operates on. The leveled path is also the one that
        // runs `with_compaction_applied` (sort + invariant guard), so the artifact
        // reflects the code E1 is meant to vet.
        for (name, tree) in [
            ("spatial", &engine.spatial),
            ("dictionary", &engine.dictionary),
        ] {
            tree.seal_active();
            tree.flush_sealed().expect("flush");
            let mut passes = 0;
            while tree.maybe_compact().expect("compaction pass") {
                passes += 1;
                assert!(passes < 500, "{name}: compaction did not converge");
            }
            let counts = tree.level_table_counts();
            assert!(
                l1_plus_tables(&counts) >= 2,
                "{name}: L1+ holds {} table(s) (levels: {counts:?}) after {passes} \
                 passes. A control needs a MULTI-table L1+ run: with one table the \
                 per-level binary search has nothing to get wrong, so zero findings \
                 would prove nothing. Lower TURBA_TEST_MEMTABLE_BYTES or raise RECORDS.",
                l1_plus_tables(&counts)
            );
            println!("  {name}: levels {counts:?} after {passes} compaction passes");
        }

        // Graceful close: the method law is to inspect after a clean shutdown +
        // reopen, never on a process that just finished writing.
        engine.shutdown().expect("graceful shutdown");
    }

    // ── Verify after a clean reopen ───────────────────────────────────────────
    // The artifact is only useful if it still presents L1+ when loaded from the
    // manifest — that is the state E1 will actually read.
    {
        let engine = TurbaEngine::open(&path, config()).expect("reopen");
        for (name, tree) in [
            ("spatial", &engine.spatial),
            ("dictionary", &engine.dictionary),
        ] {
            let counts = tree.level_table_counts();
            assert!(
                l1_plus_tables(&counts) >= 2,
                "{name}: L1+ is not a multi-table run after a clean reopen \
                 (levels: {counts:?}) — the artifact does not survive as the \
                 structure it is meant to test"
            );
            println!("  after reopen — {name}: levels {counts:?}");
        }
        // The corpse must be HEALTHY: no invariant guard may have fired while
        // building it, or the negative control would ship pre-poisoned.
        assert_eq!(
            turba_engine::tree::version::level_overlap_violations(),
            0,
            "an invariant guard fired while generating the corpse — it is not a \
             healthy control. Per-keyspace: {:?}",
            turba_engine::tree::version::level_overlap_by_keyspace()
        );
        engine.shutdown().expect("graceful shutdown after verify");
    }

    println!("healthy compacted corpse ready at {dir}");
}
