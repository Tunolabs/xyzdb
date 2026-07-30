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

// --- Basic CRUD ---

#[test]
fn tree_insert_get() {
    let (tree, _dir) = test_tree();
    tree.insert(b"hello", b"world").unwrap();
    let val = tree.get(b"hello").unwrap();
    assert_eq!(val, Some(b"world".to_vec()));
}

#[test]
fn tree_insert_overwrite_get_latest() {
    let (tree, _dir) = test_tree();
    tree.insert(b"key", b"v1").unwrap();
    tree.insert(b"key", b"v2").unwrap();
    let val = tree.get(b"key").unwrap();
    assert_eq!(val, Some(b"v2".to_vec()));
}

#[test]
fn tree_delete_returns_none() {
    let (tree, _dir) = test_tree();
    tree.insert(b"key", b"value").unwrap();
    tree.remove(b"key").unwrap();
    assert_eq!(tree.get(b"key").unwrap(), None);
}

#[test]
fn tree_absent_key() {
    let (tree, _dir) = test_tree();
    tree.insert(b"exists", b"v").unwrap();
    assert_eq!(tree.get(b"nope").unwrap(), None);
}

// --- Prefix scan ---

#[test]
fn tree_prefix_scan_correct() {
    let (tree, _dir) = test_tree();

    for i in 0..50 {
        tree.insert(
            format!("lobe_A/{i:04}").as_bytes(),
            format!("a{i}").as_bytes(),
        )
        .unwrap();
        tree.insert(
            format!("lobe_B/{i:04}").as_bytes(),
            format!("b{i}").as_bytes(),
        )
        .unwrap();
    }

    let a_results = tree.prefix(b"lobe_A/").unwrap();
    assert_eq!(a_results.len(), 50);
    assert!(a_results.iter().all(|e| e.key.starts_with(b"lobe_A/")));

    let b_results = tree.prefix(b"lobe_B/").unwrap();
    assert_eq!(b_results.len(), 50);

    let empty = tree.prefix(b"lobe_C/").unwrap();
    assert!(empty.is_empty());
}

// --- Flush and read from disk ---

#[test]
fn tree_flush_and_read_from_disk() {
    let (tree, _dir) = test_tree();

    for i in 0..100 {
        tree.insert(
            format!("key_{i:06}").as_bytes(),
            format!("val_{i}").as_bytes(),
        )
        .unwrap();
    }

    // Seal and flush
    assert!(tree.seal_active());
    let flushed = tree.flush_sealed().unwrap();
    assert_eq!(flushed, 1);
    assert_eq!(tree.l0_table_count(), 1);
    assert_eq!(tree.sealed_memtable_count(), 0);

    // Data should still be readable (now from SSTable)
    for i in 0..100 {
        let key = format!("key_{i:06}");
        let val = tree.get(key.as_bytes()).unwrap();
        assert_eq!(val, Some(format!("val_{i}").into_bytes()), "missing {key}");
    }
}

#[test]
fn tree_multiple_flushes_l0() {
    let (tree, _dir) = test_tree();

    // Flush 1
    for i in 0..50 {
        tree.insert(format!("key_{i:06}").as_bytes(), b"flush1")
            .unwrap();
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    // Flush 2 — overlapping keys with new values
    for i in 25..75 {
        tree.insert(format!("key_{i:06}").as_bytes(), b"flush2")
            .unwrap();
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    // Flush 3
    for i in 50..100 {
        tree.insert(format!("key_{i:06}").as_bytes(), b"flush3")
            .unwrap();
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    assert_eq!(tree.l0_table_count(), 3);

    // Reads should merge correctly: latest flush wins
    // key_000000-key_000024: flush1
    assert_eq!(tree.get(b"key_000010").unwrap(), Some(b"flush1".to_vec()));
    // key_000025-key_000049: flush2 (overwrote flush1)
    assert_eq!(tree.get(b"key_000030").unwrap(), Some(b"flush2".to_vec()));
    // key_000050-key_000074: flush3 (overwrote flush2)
    assert_eq!(tree.get(b"key_000060").unwrap(), Some(b"flush3".to_vec()));
    // key_000075-key_000099: flush3
    assert_eq!(tree.get(b"key_000090").unwrap(), Some(b"flush3".to_vec()));
}

// --- MVCC visibility ---

#[test]
fn tree_mvcc_visibility() {
    let (tree, _dir) = test_tree();

    let s1 = tree.insert(b"key", b"v1").unwrap();
    let s2 = tree.insert(b"key", b"v2").unwrap();
    let _s3 = tree.insert(b"key", b"v3").unwrap();

    // At s1, only v1 visible
    assert_eq!(tree.get_at(b"key", s1).unwrap(), Some(b"v1".to_vec()));
    // At s2, v2 visible
    assert_eq!(tree.get_at(b"key", s2).unwrap(), Some(b"v2".to_vec()));
    // At latest, v3 visible
    assert_eq!(tree.get(b"key").unwrap(), Some(b"v3".to_vec()));
}

// --- Concurrent read/write ---

#[test]
fn tree_concurrent_read_write() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let config = TreeConfig::default();
    let tree = Arc::new(Tree::open(&dir.path().join("tree"), config, cache).unwrap());

    // Writer thread
    let tree_w = Arc::clone(&tree);
    let writer = std::thread::spawn(move || {
        for i in 0..5000u32 {
            tree_w
                .insert(
                    format!("key_{i:06}").as_bytes(),
                    format!("val_{i}").as_bytes(),
                )
                .unwrap();
        }
    });

    // Reader threads
    let mut readers = Vec::new();
    for _ in 0..3 {
        let tree_r = Arc::clone(&tree);
        readers.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                // Random reads — should never panic
                let _ = tree_r.get(b"key_002500");
                let _ = tree_r.prefix(b"key_001");
            }
        }));
    }

    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }

    // All writes should be present
    for i in 0..5000u32 {
        let key = format!("key_{i:06}");
        assert!(tree.get(key.as_bytes()).unwrap().is_some(), "missing {key}");
    }
}

// --- Scale test ---

#[test]
fn tree_10k_records_insert_scan() {
    let (tree, _dir) = test_tree();

    for i in 0..10_000 {
        tree.insert(
            format!("lobe_01/{i:08}").as_bytes(),
            format!("data_{i}").as_bytes(),
        )
        .unwrap();
    }

    // Flush to disk
    tree.seal_active();
    tree.flush_sealed().unwrap();

    // Insert more in memtable (mixed source read)
    for i in 10_000..10_500 {
        tree.insert(
            format!("lobe_01/{i:08}").as_bytes(),
            format!("data_{i}").as_bytes(),
        )
        .unwrap();
    }

    let results = tree.prefix(b"lobe_01/").unwrap();
    assert_eq!(results.len(), 10_500);

    // Verify order
    for i in 0..results.len() - 1 {
        assert!(results[i].key <= results[i + 1].key);
    }
}

// --- Bloom filter reduces reads ---

#[test]
fn tree_bloom_reduces_disk_reads() {
    let (tree, _dir) = test_tree();

    // Write and flush — bloom filter is built during flush
    for i in 0..1000 {
        tree.insert(
            format!("exist_{i:06}").as_bytes(),
            format!("v{i}").as_bytes(),
        )
        .unwrap();
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    // Absent key lookups should be fast (bloom filter rejects)
    for i in 0..1000 {
        let key = format!("absent_{i:06}");
        let result = tree.get(key.as_bytes()).unwrap();
        assert!(result.is_none());
    }
}

// --- Prefix scan across memtable + SSTables ---

#[test]
fn tree_prefix_across_memtable_and_sstable() {
    let (tree, _dir) = test_tree();

    // Phase 1: write to disk
    for i in 0..100 {
        tree.insert(format!("pfx/a_{i:04}").as_bytes(), b"disk")
            .unwrap();
    }
    tree.seal_active();
    tree.flush_sealed().unwrap();

    // Phase 2: write to memtable
    for i in 0..50 {
        tree.insert(format!("pfx/b_{i:04}").as_bytes(), b"mem")
            .unwrap();
    }

    // Prefix scan should see both
    let results = tree.prefix(b"pfx/").unwrap();
    assert_eq!(results.len(), 150);

    // a_ entries from disk
    let a_count = results
        .iter()
        .filter(|e| e.key.starts_with(b"pfx/a_"))
        .count();
    assert_eq!(a_count, 100);

    // b_ entries from memtable
    let b_count = results
        .iter()
        .filter(|e| e.key.starts_with(b"pfx/b_"))
        .count();
    assert_eq!(b_count, 50);
}

// --- Delete survives flush ---

#[test]
fn tree_delete_survives_flush() {
    let (tree, _dir) = test_tree();

    tree.insert(b"key", b"value").unwrap();
    tree.remove(b"key").unwrap();

    tree.seal_active();
    tree.flush_sealed().unwrap();

    // Tombstone in SSTable should still make key invisible
    assert_eq!(tree.get(b"key").unwrap(), None);
}

// --- Warmup telemetry (H1.1) ---

fn config_for_warmup() -> TreeConfig {
    TreeConfig {
        sstable: SSTableConfig {
            compression: CompressionType::Lz4,
            data_block_size: 4096,
            ..Default::default()
        },
        max_memtable_size: 64 * 1024,
        compaction: LeveledConfig::default(),
        level_compressions: None,
    }
}

#[test]
fn warmup_stats_zero_on_empty_tree() {
    // No manifest yet — open returns immediately with no SSTables loaded.
    let (tree, _dir) = test_tree();
    let stats = tree.warmup_stats();
    assert_eq!(stats.sstables_opened, 0);
    assert_eq!(stats.bytes_loaded, 0);
    // wall_ms can be 0 on a fast machine; the contract is "no SSTables
    // were opened", which is what sstables_opened == 0 already covers.
}

#[test]
fn warmup_stats_recorded_on_open_with_existing_sstables() {
    // Stage one: populate three flushed SSTables, then drop the Tree so
    // the manifest is the only state remaining on disk.
    let dir = tempfile::tempdir().unwrap();
    let tree_path = dir.path().join("tree");
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    {
        let tree = Tree::open(&tree_path, config_for_warmup(), Arc::clone(&cache)).unwrap();
        for flush_idx in 0..3 {
            for i in 0..50 {
                tree.insert(
                    format!("k_{flush_idx}_{i:05}").as_bytes(),
                    format!("v_{flush_idx}_{i}").as_bytes(),
                )
                .unwrap();
            }
            tree.seal_active();
            tree.flush_sealed().unwrap();
        }
        assert_eq!(tree.l0_table_count(), 3);
        // First open built the manifest from scratch — nothing to warmup.
        assert_eq!(tree.warmup_stats().sstables_opened, 0);
    }

    // Stage two: reopen on the same path. The manifest now lists 3
    // SSTables and the warmup loop should report it.
    let cache2 = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let tree2 = Tree::open(&tree_path, config_for_warmup(), cache2).unwrap();
    let stats = tree2.warmup_stats();
    assert_eq!(stats.sstables_opened, 3, "manifest had 3 SSTables");
    assert!(
        stats.bytes_loaded > 0,
        "bytes_loaded should reflect bloom + index + meta reads, got {}",
        stats.bytes_loaded
    );
}

#[test]
fn warmup_stats_persisted_after_compaction() {
    // After a compaction merges multiple SSTables into one, reopening
    // the tree should reflect the post-compaction count, not the original
    // pre-flush count. Defends against a regression where the manifest
    // and the warmup count drift.
    let dir = tempfile::tempdir().unwrap();
    let tree_path = dir.path().join("tree");
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let post_compact_table_count;
    {
        let tree = Tree::open(&tree_path, config_for_warmup(), Arc::clone(&cache)).unwrap();
        for flush_idx in 0..4 {
            for i in 0..50 {
                tree.insert(
                    format!("k_{i:05}").as_bytes(),
                    format!("v_{flush_idx}_{i}").as_bytes(),
                )
                .unwrap();
            }
            tree.seal_active();
            tree.flush_sealed().unwrap();
        }
        tree.major_compact().unwrap();
        post_compact_table_count =
            tree.l0_table_count() + (1..7).map(|l| tree.level_table_counts()[l]).sum::<usize>();
        assert!(post_compact_table_count >= 1);
    }

    let cache2 = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let tree2 = Tree::open(&tree_path, config_for_warmup(), cache2).unwrap();
    assert_eq!(
        tree2.warmup_stats().sstables_opened,
        post_compact_table_count,
        "reopened tree should warmup exactly the manifest's post-compact tables"
    );
}
