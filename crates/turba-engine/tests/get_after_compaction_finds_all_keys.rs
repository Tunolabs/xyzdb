//! Regression for the silent point-lookup false-negative at scale.
//!
//! `Tree::get_at` binary-searches each L1+ level on the table `[key_min,
//! key_max]` ranges, which is only correct if the level is sorted by key_min
//! (the documented L1+ invariant). `Version::with_compaction_applied` used to
//! `extend` (append) freshly merged tables — whose key range belongs in the
//! MIDDLE of the level — at the END, leaving the level unsorted after the first
//! mid-range compaction. Point lookups then MISSED present keys (range scans,
//! which don't binary-search, still found them — which is why this surfaced
//! only as silent empty ghost reads at Scale 1.0, never as an error).
//!
//! This builds a tree whose data spreads across multi-table L1+ levels after a
//! major compaction, then asserts EVERY inserted key is found by `get` — the
//! point-lookup path. Pre-fix, middle-range keys come back `None`.

use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

#[test]
fn get_finds_every_key_after_multi_table_level_compaction() {
    const KEYS: usize = 20_000;
    const STRIDE: u64 = 0x9E3779B97F4A7C15; // golden ratio — scatter across the space
    const FLUSH_EVERY: usize = 1_000;

    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(16 * 1024 * 1024));
    let config = TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: 16 * 1024,
        compaction: LeveledConfig {
            max_l0_tables: 4,
            base_size_bytes: 32 * 1024,
            size_ratio: 10,
            target_table_size: 16 * 1024, // small → many tables per level
            ..Default::default()
        },
        level_compressions: None,
    };
    let tree = Tree::open(&dir.path().join("t"), config, cache).unwrap();

    // Insert KEYS distinct keys scattered across the whole 64-bit space, so the
    // levels end up holding many tables whose ranges interleave the insertion
    // order (forcing mid-range compaction merges).
    let key_of =
        |i: usize| -> Vec<u8> { format!("k{:016x}", (i as u64).wrapping_mul(STRIDE)).into_bytes() };
    for i in 0..KEYS {
        tree.insert(&key_of(i), format!("v{i}").as_bytes()).unwrap();
        if (i + 1) % FLUSH_EVERY == 0 {
            tree.seal_active();
            tree.flush_sealed().unwrap();
        }
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    tree.major_compact().unwrap();
    eprintln!("levels after compaction: {:?}", tree.level_table_counts());

    // Every key must be found by the point-lookup path.
    let mut misses = Vec::new();
    for i in 0..KEYS {
        match tree.get(&key_of(i)).unwrap() {
            Some(v) => assert_eq!(v, format!("v{i}").into_bytes(), "wrong value for key {i}"),
            None => misses.push(i),
        }
    }
    assert!(
        misses.is_empty(),
        "{} of {KEYS} keys MISSED by get despite being present (point-lookup \
         binary_search over an unsorted level) — first few: {:?}",
        misses.len(),
        &misses[..misses.len().min(10)]
    );
}
