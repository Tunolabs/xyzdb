use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compression::CompressionType;
use turba_engine::flush;
use turba_engine::memtable::Memtable;
use turba_engine::table::reader::SSTableReader;
use turba_engine::table::writer::SSTableConfig;
use turba_engine::types::ValueType;

#[test]
fn memtable_insert_get() {
    let mt = Memtable::new();
    mt.insert(b"hello", b"world", 1, ValueType::Value);

    let result = mt.get(b"hello", u64::MAX);
    assert!(result.is_some());
    let (vtype, val) = result.unwrap();
    assert_eq!(vtype, ValueType::Value);
    assert_eq!(val, b"world");
}

#[test]
fn memtable_overwrite() {
    let mt = Memtable::new();
    mt.insert(b"key", b"v1", 1, ValueType::Value);
    mt.insert(b"key", b"v2", 2, ValueType::Value);

    // Latest seqno wins
    let (_, val) = mt.get(b"key", u64::MAX).unwrap();
    assert_eq!(val, b"v2");

    // At seqno=1, only v1 visible
    let (_, val) = mt.get(b"key", 1).unwrap();
    assert_eq!(val, b"v1");
}

#[test]
fn memtable_tombstone() {
    let mt = Memtable::new();
    mt.insert(b"key", b"value", 1, ValueType::Value);
    mt.insert(b"key", b"", 2, ValueType::Tombstone);

    let (vtype, _) = mt.get(b"key", u64::MAX).unwrap();
    assert_eq!(vtype, ValueType::Tombstone);

    // Before tombstone, value is visible
    let (vtype, val) = mt.get(b"key", 1).unwrap();
    assert_eq!(vtype, ValueType::Value);
    assert_eq!(val, b"value");
}

#[test]
fn memtable_absent_key() {
    let mt = Memtable::new();
    mt.insert(b"aaa", b"v", 1, ValueType::Value);
    assert!(mt.get(b"zzz", u64::MAX).is_none());
}

#[test]
fn memtable_iter_sorted() {
    let mt = Memtable::new();
    // Insert in random-ish order
    mt.insert(b"cherry", b"3", 3, ValueType::Value);
    mt.insert(b"apple", b"1", 1, ValueType::Value);
    mt.insert(b"banana", b"2", 2, ValueType::Value);

    let entries: Vec<_> = mt.iter().collect();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].key, b"apple");
    assert_eq!(entries[1].key, b"banana");
    assert_eq!(entries[2].key, b"cherry");
}

#[test]
fn memtable_iter_seqno_desc() {
    let mt = Memtable::new();
    mt.insert(b"key", b"old", 1, ValueType::Value);
    mt.insert(b"key", b"new", 5, ValueType::Value);
    mt.insert(b"key", b"mid", 3, ValueType::Value);

    let entries: Vec<_> = mt.iter().collect();
    assert_eq!(entries.len(), 3);
    // Same key: highest seqno first (DESC)
    assert_eq!(entries[0].seqno, 5);
    assert_eq!(entries[1].seqno, 3);
    assert_eq!(entries[2].seqno, 1);
}

#[test]
fn memtable_concurrent_insert() {
    let mt = Arc::new(Memtable::new());
    let mut handles = Vec::new();

    for thread_id in 0..4u32 {
        let mt = Arc::clone(&mt);
        handles.push(std::thread::spawn(move || {
            for i in 0..1000u32 {
                let key = format!("t{thread_id}_k{i:06}");
                let val = format!("v{i}");
                mt.insert(
                    key.as_bytes(),
                    val.as_bytes(),
                    (thread_id * 1000 + i + 1) as u64,
                    ValueType::Value,
                );
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(mt.len(), 4000);

    // Spot check each thread's data
    for thread_id in 0..4u32 {
        let key = format!("t{thread_id}_k000500");
        assert!(mt.get(key.as_bytes(), u64::MAX).is_some());
    }
}

#[test]
fn memtable_size_tracking() {
    let mt = Memtable::new();
    assert_eq!(mt.approximate_size(), 0);

    mt.insert(b"key1", b"value1", 1, ValueType::Value);
    let s1 = mt.approximate_size();
    assert!(s1 > 0);

    mt.insert(b"key2", b"value2", 2, ValueType::Value);
    assert!(mt.approximate_size() > s1);
}

// --- Flush tests ---

#[test]
fn flush_produces_valid_sstable() {
    let mt = Memtable::new();
    for i in 0..500u32 {
        let key = format!("key_{i:06}");
        let val = format!("value_{i}");
        mt.insert(
            key.as_bytes(),
            val.as_bytes(),
            (i + 1) as u64,
            ValueType::Value,
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flush.sst");
    let config = SSTableConfig {
        compression: CompressionType::Lz4,
        ..Default::default()
    };

    let meta = flush::flush_memtable(&mt, &path, 1, &config).unwrap();
    assert!(meta.is_some());
    let meta = meta.unwrap();
    assert_eq!(meta.item_count, 500);

    // Read back and verify
    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let reader = SSTableReader::open(&path, cache).unwrap();

    for i in 0..500u32 {
        let key = format!("key_{i:06}");
        let result = reader.get(key.as_bytes(), u64::MAX).unwrap();
        assert!(result.is_some(), "missing {key}");
        let entry = result.unwrap();
        assert_eq!(entry.value, format!("value_{i}").as_bytes());
    }
}

#[test]
fn flush_empty_memtable() {
    let mt = Memtable::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.sst");

    let meta = flush::flush_memtable(&mt, &path, 1, &SSTableConfig::default()).unwrap();
    assert!(meta.is_none());
}

#[test]
fn flush_preserves_order() {
    let mt = Memtable::new();
    // Insert in reverse order — memtable sorts internally
    for i in (0..200u32).rev() {
        let key = format!("key_{i:06}");
        mt.insert(key.as_bytes(), b"v", (i + 1) as u64, ValueType::Value);
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ordered.sst");
    let config = SSTableConfig::default();

    flush::flush_memtable(&mt, &path, 1, &config).unwrap();

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let reader = SSTableReader::open(&path, cache).unwrap();

    let all = reader.scan().unwrap();
    assert_eq!(all.len(), 200);
    // Verify sorted order
    for i in 0..199 {
        assert!(
            all[i].key <= all[i + 1].key,
            "out of order at {i}: {:?} > {:?}",
            String::from_utf8_lossy(&all[i].key),
            String::from_utf8_lossy(&all[i + 1].key),
        );
    }
}

#[test]
fn flush_with_tombstones() {
    let mt = Memtable::new();
    mt.insert(b"key_a", b"val_a", 1, ValueType::Value);
    mt.insert(b"key_b", b"", 2, ValueType::Tombstone);
    mt.insert(b"key_c", b"val_c", 3, ValueType::Value);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tombstone.sst");

    flush::flush_memtable(&mt, &path, 1, &SSTableConfig::default()).unwrap();

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let reader = SSTableReader::open(&path, cache).unwrap();

    let all = reader.scan().unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[1].value_type, ValueType::Tombstone);

    assert_eq!(reader.meta().tombstone_count, 1);
}
