//! G3 gate (0.9 Fase 1) — the sequential read-ahead HINT is emitted correctly.
//!
//! G3 is a kernel hint (`F_RDADVISE` on Darwin / `POSIX_FADV_SEQUENTIAL` on
//! Linux): it changes no bits, no data, and allocates nothing (the read-ahead
//! lands in the KERNEL page cache), so the usual hard signals — the bit gate,
//! recall, allocation accounting — cannot see it. Its magnitude
//! (`pread_service_time_buckets` shifting faster, page-cache residency leading
//! the scan) is a latency/IO effect that only shows on HDD/x86 and is verified
//! in the close block — **pending-x86, by design, not a hole.**
//!
//! What IS deterministic on the Mac, and what this gate asserts: the hint is
//! EMITTED on the right byte range, ONLY for bulk scans (never point lookups),
//! and WITHOUT error on the scan's fd. This is a behavioural test, not a
//! performance test.

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

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

/// Insert `n` entries `k{i:06}` (256-byte values) into ONE flushed SSTable so a
/// full scan is a single bulk iterator over a known contiguous byte extent.
fn flushed_tree(dir: &std::path::Path, n: usize) -> (Tree, Arc<BlockCache>) {
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let tree = Tree::open(dir, config(), Arc::clone(&cache)).unwrap();
    for i in 0..n {
        let k = format!("k{i:06}").into_bytes();
        let mut v = vec![0u8; 256];
        v[..8].copy_from_slice(&(i as u64).to_le_bytes());
        tree.insert(&k, &v).unwrap();
    }
    assert!(tree.seal_active());
    tree.flush_sealed().unwrap();
    (tree, cache)
}

#[test]
fn readahead_hint_fires_only_on_bulk_scans_over_the_correct_range() {
    let dir = tempfile::tempdir().unwrap();
    let (tree, cache) = flushed_tree(&dir.path().join("t"), 3000);

    // Point lookup: no bulk iterator ⇒ NO read-ahead hint.
    assert!(tree.get(b"k001500").unwrap().is_some(), "key must exist");
    assert_eq!(
        cache.readahead_hint_stats(),
        (0, 0),
        "a point lookup must not emit a read-ahead hint"
    );

    // Full bulk scan: emits the hint, no error, over a non-empty extent.
    let n_full = tree.range_stream(LO, HI).unwrap().count();
    assert_eq!(n_full, 3000);
    let (ok_full, err_full) = cache.readahead_hint_stats();
    assert!(ok_full >= 1, "a bulk scan must emit a read-ahead hint");
    assert_eq!(
        err_full, 0,
        "the hint must not error on the scan's fd/range"
    );
    let (off_full, len_full) = cache.last_readahead_range();
    assert!(len_full > 0, "the hinted range must be non-empty");
    let _ = off_full;

    // Narrower bulk scan: the hint tracks the ACTUAL [first, last) extent, so a
    // ~100-key window hints a strictly smaller range than the full sweep — proof
    // the range is computed from the scan bounds, not a hardcoded whole-file advise.
    let _ = tree.range_stream(b"k000000", b"k000100").unwrap().count();
    let (_off_sub, len_sub) = cache.last_readahead_range();
    assert!(len_sub > 0, "sub-range hint must be non-empty");
    assert!(
        len_sub < len_full,
        "a narrower scan hints a smaller range (sub {len_sub} < full {len_full})"
    );

    // Still no errors after both scans.
    assert_eq!(cache.readahead_hint_stats().1, 0, "no hint may error");
}
