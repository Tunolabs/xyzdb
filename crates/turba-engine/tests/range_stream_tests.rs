//! Gates for `Tree::range_stream` — the lazy, INCLUSIVE `[start, end]` scan that
//! NEAREST's fused bucket sweep uses instead of `range` (which materialized the
//! whole bucket = the query balloon that OOM'd 100k+).
//!
//! Two properties must hold, both bit-exact:
//!   1. `range_stream(a, b)` yields EXACTLY the visible entries in `[a, b]`, in
//!      key order — proven against an independent `BTreeMap` oracle, across the
//!      memtable + flushed SSTables (so streaming really spans blocks).
//!   2. The upper bound is INCLUSIVE — an entry sitting exactly on `end` is
//!      returned, and the first key of the next gravity bucket is not. This is
//!      the off-by-one guard: NEAREST's `key_max` is the saturated all-`0xFF`
//!      tail of a bucket, so a HALF-OPEN bound (`range_iter`) would silently drop
//!      a record on it — a recall bug invisible until seq/z-order saturate.

use std::collections::BTreeMap;
use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compaction::leveled::LeveledConfig;
use turba_engine::compression::CompressionType;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::tree::{Tree, TreeConfig};

fn test_tree() -> (Tree, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let config = TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: 64 * 1024,
        compaction: LeveledConfig::default(),
        level_compressions: None,
    };
    let tree = Tree::open(&dir.path().join("tree"), config, cache).unwrap();
    (tree, dir)
}

/// `(key, value)` pairs a `range_stream` yielded — the comparison unit.
fn collect(tree: &Tree, a: &[u8], b: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    tree.range_stream(a, b)
        .unwrap()
        .map(|e| (e.key.clone(), e.value.clone()))
        .collect()
}

/// The independent oracle: the inclusive `[a, b]` slice of a sorted map.
fn oracle(map: &BTreeMap<Vec<u8>, Vec<u8>>, a: &[u8], b: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    map.range(a.to_vec()..=b.to_vec())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[test]
fn range_stream_matches_oracle_across_memtable_and_sstable() {
    let (tree, _dir) = test_tree();
    let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    // Batch 1 → flush to an SSTable.
    for i in 0..200u32 {
        let k = format!("k{i:05}").into_bytes();
        let v = format!("v1_{i}").into_bytes();
        tree.insert(&k, &v).unwrap();
        want.insert(k, v);
    }
    assert!(tree.seal_active());
    tree.flush_sealed().unwrap();

    // Batch 2 → overlapping keys with NEW values (overwrite) + fresh keys, left
    // in the active memtable. So a scan must merge SSTable + memtable and honour
    // latest-wins — exactly the fused NEAREST read shape.
    for i in 100..300u32 {
        let k = format!("k{i:05}").into_bytes();
        let v = format!("v2_{i}").into_bytes();
        tree.insert(&k, &v).unwrap();
        want.insert(k, v);
    }
    // One deletion — the tombstone must be filtered out of the stream.
    tree.remove(b"k00050").unwrap();
    want.remove(b"k00050".as_slice());

    // Full range + several sub-ranges (incl. bounds that fall between keys).
    let full_lo = b"k00000".to_vec();
    let full_hi = b"k99999".to_vec();
    for (a, b) in [
        (full_lo.as_slice(), full_hi.as_slice()),
        (b"k00100".as_slice(), b"k00199".as_slice()),
        (b"k00095".as_slice(), b"k00205".as_slice()),
        (b"k00250".as_slice(), b"k99999".as_slice()),
    ] {
        assert_eq!(
            collect(&tree, a, b),
            oracle(&want, a, b),
            "range_stream != oracle for [{:?}, {:?}]",
            String::from_utf8_lossy(a),
            String::from_utf8_lossy(b),
        );
    }
}

// ─── Off-by-one: the inclusive upper bound (the recall-breaking risk) ─────────

/// A 22-byte gravity-bucket key: [lobe_id BE(2)][gravity_hash u48 BE(6)][tail(14)].
/// Mirrors `xyzdb_core::SpatialKey` layout without depending on the upper crate.
fn gkey(lobe: u16, gh: u64, tail: [u8; 14]) -> Vec<u8> {
    let mut k = Vec::with_capacity(22);
    k.extend_from_slice(&lobe.to_be_bytes());
    k.extend_from_slice(&gh.to_be_bytes()[2..8]); // low 48 bits, BE
    k.extend_from_slice(&tail);
    k
}

/// `prefix_for_gravity`: key_min zeroes the tail, key_max saturates it to 0xFF.
fn bucket_bounds(lobe: u16, gh: u64) -> (Vec<u8>, Vec<u8>) {
    (gkey(lobe, gh, [0x00; 14]), gkey(lobe, gh, [0xFF; 14]))
}

fn assert_inclusive_bound(flush: bool) {
    let (tree, _dir) = test_tree();
    let (key_min, key_max) = bucket_bounds(1, 5);

    // Three entries in bucket gh=5: a middle key, and one EXACTLY on key_max
    // (saturated tail) — the entry a half-open bound would drop.
    let mid = gkey(1, 5, {
        let mut t = [0u8; 14];
        t[13] = 0x2A;
        t
    });
    let on_max = key_max.clone(); // tail all 0xFF == key_max
    // First key of the NEXT bucket (gh=6, zero tail) = key_max(gh5) + 1. Must be
    // EXCLUDED — inclusive `[min, max]` stops at gh=5's saturated tail.
    let next_bucket = gkey(1, 6, [0x00; 14]);

    tree.insert(&mid, b"mid").unwrap();
    tree.insert(&on_max, b"on_max").unwrap();
    tree.insert(&next_bucket, b"next").unwrap();
    if flush {
        assert!(tree.seal_active());
        tree.flush_sealed().unwrap();
    }

    let got = collect(&tree, &key_min, &key_max);
    let keys: Vec<&[u8]> = got.iter().map(|(k, _)| k.as_slice()).collect();

    assert!(
        keys.contains(&mid.as_slice()),
        "flush={flush}: middle entry missing"
    );
    assert!(
        keys.contains(&on_max.as_slice()),
        "flush={flush}: OFF-BY-ONE — entry on key_max (saturated tail) was dropped; \
         a half-open [min, max) bound would silently lose the last bucket record"
    );
    assert!(
        !keys.contains(&next_bucket.as_slice()),
        "flush={flush}: next-bucket key leaked into the range (upper bound not tight)"
    );
    assert_eq!(
        got.len(),
        2,
        "flush={flush}: expected exactly {{mid, on_max}}"
    );
}

#[test]
fn range_stream_upper_bound_inclusive_memtable() {
    assert_inclusive_bound(false);
}

#[test]
fn range_stream_upper_bound_inclusive_sstable() {
    assert_inclusive_bound(true);
}
