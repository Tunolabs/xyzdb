//! G2 gates (0.9 Fase 1) — the block-cache `Scan` lane and its span threshold.
//!
//! Three properties the work order singled out:
//!   1. Gray zone ADMITS (zero regression): a scan whose on-disk span fits the
//!      cache keeps `UserIORead` and populates the cache exactly as before — the
//!      second pass is served entirely from cache. The `Scan` lane stays at zero.
//!   2. Bypass touches only thrashers; the hot set survives: a scan whose span
//!      exceeds the cache routes through `Lane::Scan` (skipped, admits nothing),
//!      so a coexisting hot working set is NOT evicted by the sweep.
//!   3. `Lane::COUNT` sizes every per-lane array: indexing each lane is in-bounds
//!      (the silent-OOB guard the work order flagged).
//!
//! Blocks are stored UNCOMPRESSED so the on-disk span ≈ entry bytes, making the
//! span-vs-capacity threshold deterministic (LZ4 on patterned values would
//! shrink the span unpredictably). `Tree::insert` does not auto-flush, so each
//! `seal_active` + `flush_sealed` yields exactly one SSTable — the per-SSTable
//! span (what the G2 threshold measures) is then the whole dataset.

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::io::Lane;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

const USER_IDX: usize = 0; // Lane::UserIORead.index()
const SCAN_IDX: usize = 4; // Lane::Scan.index()
const LO: &[u8] = b"k000000";
const HI: &[u8] = b"k999999";

fn config() -> TreeConfig {
    TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::None,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: 64 * 1024,
        compaction: LeveledConfig::default(),
        level_compressions: None,
    }
}

/// Insert `n` entries `k{i:06}` with `value_size`-byte values and flush ALL to a
/// single SSTable (the read source is disk ⇒ the block-cache path). Returns the
/// tree and its cache so the test can read the admission counters. Caches are
/// kept in quick_cache's well-behaved regime (≥ a few MiB); tiny caches evict
/// erratically at small cap/items ratios (finding H7) and would make residency
/// assertions flaky.
fn flushed_tree(
    dir: &std::path::Path,
    cap: u64,
    n: usize,
    value_size: usize,
) -> (Tree, Arc<BlockCache>) {
    let cache = Arc::new(BlockCache::new(cap));
    let tree = Tree::open(dir, config(), Arc::clone(&cache)).unwrap();
    for i in 0..n {
        let k = format!("k{i:06}").into_bytes();
        let mut v = vec![0u8; value_size];
        v[..8].copy_from_slice(&(i as u64).to_le_bytes()); // vary the head per entry
        tree.insert(&k, &v).unwrap();
    }
    assert!(tree.seal_active());
    tree.flush_sealed().unwrap();
    (tree, cache)
}

/// Gray zone: a scan that FITS the cache keeps `UserIORead`, admits every block,
/// and the second pass is served entirely from cache — identical to pre-G2.
#[test]
fn gray_zone_scan_admits_and_second_pass_is_cached() {
    let dir = tempfile::tempdir().unwrap();
    // Cache ≫ dataset span (64 MiB vs ~0.8 MiB) ⇒ the full scan fits ⇒ UserIORead.
    let (tree, cache) = flushed_tree(&dir.path().join("t"), 64 * 1024 * 1024, 3000, 256);

    // Pass 1: cold ⇒ every block missed and admitted (span ≤ capacity).
    let n1 = tree.range_stream(LO, HI).unwrap().count();
    let misses1 = cache.misses();
    let adm = cache.admission_snapshot();
    assert!(misses1 > 0, "pass 1 must be cold");
    assert!(
        adm[USER_IDX].0 >= misses1,
        "a fitting scan admits on UserIORead (admitted {} >= misses {misses1})",
        adm[USER_IDX].0
    );
    assert_eq!(
        adm[SCAN_IDX],
        (0, 0),
        "gray-zone scan must NOT touch the Scan lane (admitted, skipped)"
    );

    // Pass 2: warm ⇒ served from cache. Zero regression = the cache is populated
    // exactly as the pre-G2 UserIORead path would have populated it.
    let hits_before = cache.hits();
    let n2 = tree.range_stream(LO, HI).unwrap().count();
    let hits_delta = cache.hits() - hits_before;
    assert_eq!(n1, n2, "same entries on both passes");
    assert_eq!(
        hits_delta, misses1,
        "every block that missed on pass 1 is a cache hit on pass 2"
    );
}

/// Bypass: a scan whose span EXCEEDS the cache routes through `Lane::Scan`,
/// admits nothing (only `skipped` grows), and a coexisting hot working set is
/// NOT evicted by the sweep — re-reading it after the scan hits, no re-miss.
#[test]
fn oversized_scan_bypasses_and_hot_set_survives() {
    let dir = tempfile::tempdir().unwrap();
    // Cache 32 MiB (quick_cache's production-like regime) vs a ~48 MiB span
    // (3000 × 16 KiB, one entry per block): the full scan cannot fit ⇒ Scan lane.
    let (tree, cache) = flushed_tree(&dir.path().join("t"), 32 * 1024 * 1024, 3000, 16 * 1024);

    // Warm a small hot set via point lookups (UserIORead ⇒ always admitted).
    // Three spread-out keys ⇒ three distinct resident blocks (~12 KiB ≪ cache).
    let hot: [&[u8]; 3] = [b"k000000", b"k001000", b"k002000"];
    for k in hot {
        assert!(tree.get(k).unwrap().is_some(), "hot key must exist");
    }
    // Confirm the hot set is resident: re-reading it produces no new misses.
    let m = cache.misses();
    for k in hot {
        tree.get(k).unwrap();
    }
    assert_eq!(cache.misses(), m, "hot set must be resident after warming");

    // The bulk sweep: span > capacity ⇒ Lane::Scan ⇒ bypass admission.
    let adm0 = cache.admission_snapshot();
    let scanned = tree.range_stream(LO, HI).unwrap().count();
    assert_eq!(scanned, 3000, "scan sees every entry");
    let adm1 = cache.admission_snapshot();
    assert!(
        adm1[SCAN_IDX].1 > adm0[SCAN_IDX].1,
        "an oversized scan's cold blocks are SKIPPED on the Scan lane (skipped {} -> {})",
        adm0[SCAN_IDX].1,
        adm1[SCAN_IDX].1
    );
    assert_eq!(
        adm1[SCAN_IDX].0, adm0[SCAN_IDX].0,
        "the Scan lane admits nothing (would defeat the bypass)"
    );

    // The real benefit: the sweep admitted nothing, so it cannot have evicted
    // the hot set. Re-reading the hot set hits — no re-miss.
    let m2 = cache.misses();
    for k in hot {
        tree.get(k).unwrap();
    }
    assert_eq!(
        cache.misses(),
        m2,
        "hot working set survived the bulk scan (no re-miss)"
    );
}

/// `Lane::COUNT` sizes every per-lane array. Iterate every variant, touch
/// `index()`, and index the cache's `admission` array at each — a forgotten
/// `COUNT` bump (or a stale index) would panic here instead of silently
/// corrupting a neighbouring lane's counter.
#[test]
fn every_lane_index_is_in_bounds_no_oob() {
    let lanes = [
        Lane::UserIORead,
        Lane::WriterDurable,
        Lane::Flush,
        Lane::Compaction { target_level: 0 },
        Lane::Scan,
    ];
    assert_eq!(
        lanes.len(),
        Lane::COUNT,
        "this test must enumerate every Lane variant"
    );

    // Indices are distinct and cover exactly 0..COUNT.
    let mut seen = [false; Lane::COUNT];
    for l in lanes {
        let i = l.index();
        assert!(
            i < Lane::COUNT,
            "{l:?} index {i} out of bounds (COUNT={})",
            Lane::COUNT
        );
        assert!(!seen[i], "duplicate lane index {i} for {l:?}");
        seen[i] = true;
    }
    assert!(seen.iter().all(|&s| s), "lane indices must cover 0..COUNT");

    // The per-lane atomic arrays are sized by COUNT: indexing each lane's slot
    // must be in-bounds — this is exactly what a forgotten COUNT bump would OOB.
    let cache = BlockCache::new(1024 * 1024);
    let snap = cache.admission_snapshot();
    assert_eq!(snap.len(), Lane::COUNT);
    for l in lanes {
        let _ = snap[l.index()];
    }
}
