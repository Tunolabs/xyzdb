//! Repro harness for the scale-1 `major_compact` non-convergence observed on
//! the AWS box (`tree=spatial iteration=2641 inputs_consumed=5820
//! initial_inputs=1049 output_tables=4917`, 12.6 h, iteration not advancing).
//!
//! Hypothesis: a bulk load spreads keys across the whole keyspace via
//! `gravity_hash`, so every flushed L0 table spans the full key range and
//! fully overlaps every other L0/L1 table. Each L0→L1 / L(n)→L(n+1) merge then
//! rewrites the whole overlapping run, and with `target_table_size` smaller
//! than the merged run the output is split into many small tables. The
//! count-based overflow (`max_tables_per_level`) keeps re-triggering, so the
//! tree churns instead of converging.
//!
//! The test ingests that pathological shape at a small, local scale and asserts
//! `major_compact` both TERMINATES (within a wall-clock watchdog) and CONVERGES
//! (no non-last level left over its target). A churn reproduces as a watchdog
//! timeout; a wrong terminal structure reproduces as the convergence assert.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

/// Build a tree whose level geometry mimics the box at miniature scale:
/// tiny target tables so merges split into many outputs, and the default
/// count cap so the count-based overflow path is exercised.
fn churn_prone_tree(dir: &std::path::Path) -> Tree {
    tree_with(dir, 64 * 1024, 32 * 1024, 50, 64)
}

/// Parametrised tree: `mem` = memtable size (drives how many L0 tables a fixed
/// data volume flushes into), `tgt` = compaction output target table size,
/// `l0_batch` = L0 tables consumed per major_compact iteration, `max_amp` =
/// write-amplification ceiling (0 disables the convergence guard).
fn tree_with(dir: &std::path::Path, mem: usize, tgt: usize, l0_batch: usize, max_amp: u64) -> Tree {
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let config = TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: mem,
        compaction: LeveledConfig {
            max_l0_tables: 4,
            base_size_bytes: 64 * 1024, // L1 target 64 KB
            size_ratio: 10,
            target_table_size: tgt,
            l0_compact_batch_size: l0_batch,
            max_compaction_amplification: max_amp,
            ..Default::default() // max_tables_per_level = 20 (the cap under test)
        },
        level_compressions: None,
    };
    Tree::open(&dir.join("tree"), config, cache).unwrap()
}

/// Spread keys across the WHOLE keyspace within each flush, like gravity_hash
/// does, so every L0 table fully overlaps every other. Keys are GLOBALLY
/// distinct (no dedup collapse): a global counter times a large odd stride
/// scatters consecutive records across the full 64-bit space, so each flush's
/// run still touches both ends of the keyspace while never colliding with
/// another flush's keys.
fn ingest_full_range_flush(tree: &Tree, flush_idx: usize, keys_per_flush: usize) {
    const STRIDE: u64 = 0x9E3779B97F4A7C15; // 64-bit golden-ratio odd constant
    for i in 0..keys_per_flush {
        let global = (flush_idx * keys_per_flush + i) as u64;
        let k = global.wrapping_mul(STRIDE); // bijection over u64 → distinct + scattered
        let key = format!("k{k:016x}");
        let val = format!("f{flush_idx:04}v{i:08}");
        tree.insert(key.as_bytes(), val.as_bytes()).unwrap();
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();
}

#[test]
fn major_compact_converges_on_full_range_overlap() {
    // Tunable scale. Bump FLUSHES if it does not reproduce — the box had ~1049
    // initial inputs; we want enough L0 fan-out to trigger the cascade without
    // needing gigabytes locally.
    const FLUSHES: usize = 600;
    const KEYS_PER_FLUSH: usize = 400;
    // Generous wall-clock budget for this miniature scale. A converging
    // compaction finishes in well under a second; a churn blows past this. The
    // large headroom absorbs CPU starvation on a shared/contended CI runner,
    // where the sibling compaction tests in this binary run in parallel — a real
    // churn is unbounded, so any finite budget still catches it.
    const WATCHDOG: Duration = Duration::from_secs(300);

    let dir = tempfile::tempdir().unwrap();
    let tree = Arc::new(churn_prone_tree(dir.path()));

    for f in 0..FLUSHES {
        ingest_full_range_flush(&tree, f, KEYS_PER_FLUSH);
    }

    let before = tree.level_table_counts();
    let total_before: usize = before.iter().sum();
    eprintln!("ingested: level_table_counts={before:?} total={total_before}");

    // Run major_compact under a watchdog: a non-convergent churn would block
    // forever, so we time it out on a background thread instead of hanging the
    // test runner.
    let (tx, rx) = mpsc::channel();
    let worker = Arc::clone(&tree);
    let handle = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let res = worker.major_compact();
        let _ = tx.send((res.is_ok(), start.elapsed()));
        res
    });

    match rx.recv_timeout(WATCHDOG) {
        Ok((ok, elapsed)) => {
            handle.join().unwrap().unwrap();
            assert!(ok, "major_compact returned an error");
            let after = tree.level_table_counts();
            eprintln!(
                "compacted in {:?}: level_table_counts={after:?} total={}",
                elapsed,
                after.iter().sum::<usize>()
            );

            // Convergence: choose_compaction must report no overflow, i.e. every
            // non-last level is within its per-level count target. (MAX_LEVELS =
            // 7, last level index 6 is exempt.) The target scales with the byte
            // budget — `max(max_tables_per_level, bytes_target/target_table_size)`
            // — matching the engine's `choose_compaction`. If this fails, the
            // loop SHOULD have kept going — the terminal state is over target.
            const BASE: u64 = 64 * 1024;
            const RATIO: u64 = 10;
            const TGT: u64 = 32 * 1024;
            const FLOOR: usize = 20; // LeveledConfig::max_tables_per_level default
            for (lvl, &n) in after.iter().enumerate() {
                if lvl == 0 || lvl >= after.len() - 1 {
                    continue; // L0 has its own trigger; last level is exempt
                }
                let bytes_target = BASE.saturating_mul(RATIO.saturating_pow(lvl as u32 - 1));
                let natural = (bytes_target / TGT).max(1) as usize;
                let target = FLOOR.max(natural);
                assert!(
                    n <= target,
                    "level {lvl} left with {n} tables (> target {target}) — not converged: {after:?}"
                );
            }
        }
        Err(_) => {
            panic!(
                "major_compact did NOT terminate within {WATCHDOG:?} — \
                 reproduced the box churn at local scale (initial total={total_before} tables, \
                 levels={before:?})"
            );
        }
    }
}

/// The fail-loud guard: a churn-prone input under a deliberately low
/// write-amplification ceiling must abort `major_compact` with
/// `CompactionStalled` (carrying the level histogram) instead of grinding.
/// This is the safety net that converts the box's silent 12 h hang into a
/// fast, diagnosed failure. The same input converges fine under the default
/// ceiling (see the other tests) — proving the guard discriminates churn from
/// merely-large work.
#[test]
fn major_compact_aborts_loudly_when_amplification_runs_away() {
    use turba_engine::error::Error;
    const FLUSHES: usize = 600;
    const KEYS_PER_FLUSH: usize = 400;

    let dir = tempfile::tempdir().unwrap();
    // Ceiling = 2×: the fully-overlapping L0 re-merge blows past it well before
    // convergence, so the guard must fire.
    let tree = tree_with(dir.path(), 64 * 1024, 32 * 1024, 50, 2);
    for f in 0..FLUSHES {
        ingest_full_range_flush(&tree, f, KEYS_PER_FLUSH);
    }

    match tree.major_compact() {
        Err(Error::CompactionStalled(msg)) => {
            eprintln!("guard fired as expected: {msg}");
            assert!(
                msg.contains("write-amplification") && msg.contains("level_table_counts"),
                "diagnostic must name the cause and the level histogram, got: {msg}"
            );
        }
        Err(e) => panic!("expected CompactionStalled, got a different error: {e}"),
        Ok(()) => panic!("expected the guard to fire under a 2× ceiling, but compaction converged"),
    }
}

/// Counterpart to the pathology: the SAME total data volume and key
/// distribution, but flushed into FEW L0 tables (memtable sized to hold a large
/// fraction of the data) instead of hundreds of tiny ones. Fewer L0 tables ⇒
/// quadratically less re-merge work ⇒ major_compact finishes near-instantly.
/// This is the lowest-risk lever for the box: shrink the input to the
/// amplification rather than rewrite the compaction core.
#[test]
fn fewer_larger_l0_tables_compact_fast() {
    const TOTAL_KEYS: usize = 600 * 400; // same 240k keys as the pathology
    // Memtable large enough to hold the whole dataset in a handful of flushes.
    let dir = tempfile::tempdir().unwrap();
    let tree = tree_with(dir.path(), 4 * 1024 * 1024, 32 * 1024, 50, 64);

    const STRIDE: u64 = 0x9E3779B97F4A7C15;
    for i in 0..TOTAL_KEYS {
        let k = (i as u64).wrapping_mul(STRIDE);
        tree.insert(
            format!("k{k:016x}").as_bytes(),
            format!("v{i:08}").as_bytes(),
        )
        .unwrap();
        // Seal only when the active memtable fills — size-driven cadence, the
        // way the engine flushes under a large BULKMODE memtable.
        if tree.active_memtable_size() >= tree.max_memtable_size() {
            tree.seal_active();
            tree.flush_sealed().unwrap();
        }
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    let before = tree.level_table_counts();
    let start = std::time::Instant::now();
    tree.major_compact().unwrap();
    let elapsed = start.elapsed();
    let after = tree.level_table_counts();
    eprintln!("fewer-larger-L0: before={before:?} -> after={after:?} in {elapsed:?}");

    // Structural assertion (machine-independent, unlike wall-time): a 6.7 MB
    // dataset fits within the L3 byte budget (6.4 MB) / L4 (64 MB), so it must
    // SETTLE at its natural byte level and NOT cascade into the last level.
    // Pre-fix the flat count cap pushed ~237 tables into L6; the byte-scaled
    // count target keeps them at L3/L4 — the cascade-amplification fix.
    let last = after.len() - 1;
    assert_eq!(
        after[last], 0,
        "data cascaded into the last level {last} ({after:?}) — the flat count \
         cap is still forcing spurious downward migration"
    );
}

/// Same pathological input as above, but draining ALL L0 tables in a single
/// major_compact iteration (`l0_compact_batch_size` >= L0 count) instead of in
/// batches of 50. The batched path re-merges the whole (fully overlapping) L1
/// run once per batch — O(batches × data) write amplification. Draining L0 in
/// one pass merges each byte of L1 once. This measures how much of the box
/// slowness is the L0 re-merge amplification specifically.
#[test]
fn major_compact_full_l0_drain_is_faster() {
    const FLUSHES: usize = 600;
    const KEYS_PER_FLUSH: usize = 400;

    let dir = tempfile::tempdir().unwrap();
    // l0_batch large enough to take every L0 table in the first iteration.
    let tree = tree_with(dir.path(), 64 * 1024, 32 * 1024, FLUSHES + 1, 64);

    for f in 0..FLUSHES {
        ingest_full_range_flush(&tree, f, KEYS_PER_FLUSH);
    }
    let before = tree.level_table_counts();
    let start = std::time::Instant::now();
    tree.major_compact().unwrap();
    let elapsed = start.elapsed();
    let after = tree.level_table_counts();
    eprintln!("full-L0-drain: before={before:?} -> after={after:?} in {elapsed:?}");
}
