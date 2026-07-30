use std::sync::Arc;
use turba_engine::cache::BlockCache;
use turba_engine::compression::CompressionType;
use turba_engine::table::reader::SSTableReader;
use turba_engine::table::writer::{SSTableConfig, SSTableWriter};
use turba_engine::types::{Entry, ValueType};

fn make_entries(count: usize) -> Vec<Entry> {
    (0..count)
        .map(|i| {
            Entry::new(
                format!("key_{i:08}").into_bytes(),
                format!("value_{i}_payload_data_here").into_bytes(),
                (i + 1) as u64,
                ValueType::Value,
            )
        })
        .collect()
}

fn write_and_open(entries: &[Entry], config: SSTableConfig) -> (SSTableReader, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.sst");

    let mut writer = SSTableWriter::new(&path, 1, config).unwrap();
    for entry in entries {
        writer.add(entry.clone()).unwrap();
    }
    let meta = writer.finish().unwrap();
    assert!(meta.is_some());

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let reader = SSTableReader::open(&path, cache).unwrap();
    (reader, dir)
}

// --- Roundtrip tests ---

#[test]
fn sstable_write_read_roundtrip() {
    let entries = make_entries(1000);
    let (reader, _dir) = write_and_open(
        &entries,
        SSTableConfig {
            compression: CompressionType::Lz4,
            ..Default::default()
        },
    );

    // Point read every entry
    for entry in &entries {
        let result = reader.get(&entry.key, u64::MAX).unwrap();
        assert!(
            result.is_some(),
            "missing key {:?}",
            String::from_utf8_lossy(&entry.key)
        );
        let found = result.unwrap();
        assert_eq!(found.key, entry.key);
        assert_eq!(found.value, entry.value);
        assert_eq!(found.seqno, entry.seqno);
    }
}

#[test]
fn sstable_write_read_zstd() {
    let entries = make_entries(500);
    let (reader, _dir) = write_and_open(
        &entries,
        SSTableConfig {
            compression: CompressionType::Zstd(3),
            ..Default::default()
        },
    );

    for entry in &entries {
        let result = reader.get(&entry.key, u64::MAX).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().value, entry.value);
    }
}

// --- Bloom filter tests ---

#[test]
fn sstable_bloom_filter_works() {
    let entries = make_entries(10_000);
    let (reader, _dir) = write_and_open(
        &entries,
        SSTableConfig {
            bloom_bits_per_key: 10.0,
            compression: CompressionType::Lz4,
            ..Default::default()
        },
    );

    // All present keys should be found
    for i in [0, 100, 999, 5000, 9999] {
        let key = format!("key_{i:08}");
        let result = reader.get(key.as_bytes(), u64::MAX).unwrap();
        assert!(result.is_some(), "missing present key {key}");
    }
}

#[test]
fn sstable_bloom_rejects_absent_keys() {
    let entries = make_entries(1000);
    let (reader, _dir) = write_and_open(
        &entries,
        SSTableConfig {
            bloom_bits_per_key: 10.0,
            compression: CompressionType::Lz4,
            ..Default::default()
        },
    );

    // Absent keys should mostly be rejected by bloom
    let mut found = 0;
    for i in 0..10_000 {
        let key = format!("absent_{i:08}");
        if reader.get(key.as_bytes(), u64::MAX).unwrap().is_some() {
            found += 1;
        }
    }
    // Should find 0 (bloom may have false positives but key won't match in block)
    assert_eq!(found, 0, "found {found} absent keys — should be 0");
}

// --- Scan tests ---

#[test]
fn sstable_scan_range() {
    let entries = make_entries(1000);
    let (reader, _dir) = write_and_open(&entries, SSTableConfig::default());

    let results = reader.scan_range(b"key_00000100", b"key_00000200").unwrap();
    assert_eq!(results.len(), 100);
    assert_eq!(results[0].key, b"key_00000100");
    assert_eq!(results[99].key, b"key_00000199");
}

#[test]
fn sstable_scan_prefix() {
    // Create entries with two different prefixes
    let mut entries = Vec::new();
    for i in 0..500 {
        entries.push(Entry::new(
            format!("lobe_A/rec_{i:06}").into_bytes(),
            b"data_a".to_vec(),
            (i + 1) as u64,
            ValueType::Value,
        ));
    }
    for i in 0..500 {
        entries.push(Entry::new(
            format!("lobe_B/rec_{i:06}").into_bytes(),
            b"data_b".to_vec(),
            (i + 501) as u64,
            ValueType::Value,
        ));
    }

    let (reader, _dir) = write_and_open(&entries, SSTableConfig::default());

    let a_results = reader.scan_prefix(b"lobe_A/").unwrap();
    assert_eq!(a_results.len(), 500);
    assert!(a_results.iter().all(|e| e.key.starts_with(b"lobe_A/")));

    let b_results = reader.scan_prefix(b"lobe_B/").unwrap();
    assert_eq!(b_results.len(), 500);

    let none_results = reader.scan_prefix(b"lobe_C/").unwrap();
    assert_eq!(none_results.len(), 0);
}

// --- Cache tests ---

#[test]
fn sstable_block_cache_hit() {
    let entries = make_entries(100);
    let (reader, _dir) = write_and_open(
        &entries,
        SSTableConfig {
            data_block_size: 4096, // small blocks to get multiple
            ..Default::default()
        },
    );

    // First read: cache miss
    let r1 = reader.get(b"key_00000050", u64::MAX).unwrap();
    assert!(r1.is_some());

    // Second read of same key: should hit cache (same block)
    let r2 = reader.get(b"key_00000050", u64::MAX).unwrap();
    assert!(r2.is_some());
    assert_eq!(r1.unwrap().value, r2.unwrap().value);
}

// --- Corruption tests ---

#[test]
fn sstable_corruption_detected() {
    let entries = make_entries(100);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.sst");

    let mut writer = SSTableWriter::new(&path, 1, SSTableConfig::default()).unwrap();
    for entry in &entries {
        writer.add(entry.clone()).unwrap();
    }
    writer.finish().unwrap();

    // Corrupt a data block (byte 50 is in the first data block)
    let mut data = std::fs::read(&path).unwrap();
    data[50] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let reader = SSTableReader::open(&path, cache).unwrap();

    // Reading should detect corruption via checksum
    let result = reader.get(b"key_00000001", u64::MAX);
    assert!(result.is_err(), "should detect data block corruption");
}

#[test]
fn sstable_truncated_file_detected() {
    let entries = make_entries(100);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("truncated.sst");

    let mut writer = SSTableWriter::new(&path, 1, SSTableConfig::default()).unwrap();
    for entry in &entries {
        writer.add(entry.clone()).unwrap();
    }
    writer.finish().unwrap();

    // Truncate to just 10 bytes
    let data = std::fs::read(&path).unwrap();
    std::fs::write(&path, &data[..10]).unwrap();

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let result = SSTableReader::open(&path, cache);
    assert!(result.is_err(), "should detect truncated file");
}

#[test]
fn sstable_footer_corruption_detected() {
    let entries = make_entries(100);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad_footer.sst");

    let mut writer = SSTableWriter::new(&path, 1, SSTableConfig::default()).unwrap();
    for entry in &entries {
        writer.add(entry.clone()).unwrap();
    }
    writer.finish().unwrap();

    // Corrupt the magic bytes in footer (last 28 bytes)
    let mut data = std::fs::read(&path).unwrap();
    let footer_start = data.len() - 28;
    data[footer_start] ^= 0xFF; // corrupt magic
    std::fs::write(&path, &data).unwrap();

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let result = SSTableReader::open(&path, cache);
    assert!(result.is_err(), "should detect footer corruption");
}

// --- Disk size comparison ---

#[test]
fn sstable_lz4_vs_zstd_disk_size() {
    let entries: Vec<Entry> = (0..10_000)
        .map(|i| {
            let key = format!("lobe_0001/entity_{i:08}").into_bytes();
            let mut value = Vec::with_capacity(200);
            value.extend_from_slice(format!("nombre: Juan García #{i}").as_bytes());
            value.extend_from_slice(format!(", rfc: RFC{i:010}").as_bytes());
            value.extend_from_slice(format!(", monto: {:.2}", i as f64 * 1000.0).as_bytes());
            Entry::new(key, value, (i + 1) as u64, ValueType::Value)
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();

    // Write LZ4
    let lz4_path = dir.path().join("lz4.sst");
    let mut w = SSTableWriter::new(
        &lz4_path,
        1,
        SSTableConfig {
            compression: CompressionType::Lz4,
            ..Default::default()
        },
    )
    .unwrap();
    for e in &entries {
        w.add(e.clone()).unwrap();
    }
    w.finish().unwrap();
    let lz4_size = std::fs::metadata(&lz4_path).unwrap().len();

    // Write Zstd-3
    let zstd_path = dir.path().join("zstd.sst");
    let mut w = SSTableWriter::new(
        &zstd_path,
        2,
        SSTableConfig {
            compression: CompressionType::Zstd(3),
            ..Default::default()
        },
    )
    .unwrap();
    for e in &entries {
        w.add(e.clone()).unwrap();
    }
    w.finish().unwrap();
    let zstd_size = std::fs::metadata(&zstd_path).unwrap().len();

    eprintln!("SSTable disk size for 10K xyzDB-like entries:");
    eprintln!("  LZ4:    {lz4_size:>8} bytes");
    eprintln!(
        "  Zstd-3: {zstd_size:>8} bytes ({:.1}% of LZ4)",
        zstd_size as f64 / lz4_size as f64 * 100.0
    );

    assert!(
        zstd_size < lz4_size,
        "Zstd-3 SSTable should be smaller than LZ4"
    );
}

// --- Large values ---

#[test]
fn sstable_large_values() {
    let entries: Vec<Entry> = (0..100)
        .map(|i| {
            Entry::new(
                format!("key_{i:04}").into_bytes(),
                vec![i as u8; 10_000], // 10KB values
                (i + 1) as u64,
                ValueType::Value,
            )
        })
        .collect();

    let (reader, _dir) = write_and_open(&entries, SSTableConfig::default());

    for entry in &entries {
        let result = reader.get(&entry.key, u64::MAX).unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().value, entry.value);
    }
}

// --- Empty SSTable ---

#[test]
fn sstable_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.sst");

    let writer = SSTableWriter::new(&path, 1, SSTableConfig::default()).unwrap();
    let meta = writer.finish().unwrap();
    assert!(meta.is_none(), "empty SSTable should return None");
}

// --- Metadata ---

#[test]
fn sstable_metadata_correct() {
    let entries = make_entries(500);
    let (reader, _dir) = write_and_open(&entries, SSTableConfig::default());

    let meta = reader.meta();
    assert_eq!(meta.item_count, 500);
    assert_eq!(meta.key_min, b"key_00000000");
    assert_eq!(meta.key_max, b"key_00000499");
    assert_eq!(meta.seqno_min, 1);
    assert_eq!(meta.seqno_max, 500);
    assert_eq!(meta.tombstone_count, 0);
    assert!(meta.block_count > 0);
}

// --- Cacheable metadata: zone maps are evictable, not resident (Inc 1) ---

/// A zone-map builder that stamps each block with its first key — non-empty and
/// per-block distinct, so the test can verify the decoded blob.
struct FirstKeyZoneMapBuilder;
impl turba_engine::table::writer::ZoneMapBuilder for FirstKeyZoneMapBuilder {
    fn build_block_zone_map(&self, entries: &[Entry]) -> Vec<u8> {
        entries.first().map(|e| e.key.clone()).unwrap_or_default()
    }
}

#[test]
fn zone_maps_are_cacheable_not_resident() {
    use turba_engine::io::{Lane, Scheduler};
    use turba_engine::table::meta::decode_zone_maps;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zm.sst");
    // Tiny blocks force many blocks → many per-block zone maps.
    let config = SSTableConfig {
        data_block_size: 256,
        ..Default::default()
    };
    let entries = make_entries(2000);
    let sched = Arc::new(Scheduler::passthrough());

    let mut w = SSTableWriter::with_zone_map_builder(
        &path,
        1,
        config,
        Some(Arc::new(FirstKeyZoneMapBuilder)),
        Arc::clone(&sched),
        Lane::Flush,
    )
    .unwrap();
    for e in &entries {
        w.add(e.clone()).unwrap();
    }
    let meta = w.finish().unwrap().unwrap();
    assert!(meta.block_count > 1, "test needs a multi-block SST");

    let cache = Arc::new(BlockCache::new(64 * 1024 * 1024));
    let reader = SSTableReader::open_with_tree_id(&path, Arc::clone(&cache), 7, sched).unwrap();

    // (a) zone maps are NOT held resident in the reader's meta.
    assert!(
        reader.meta().zone_maps.is_empty(),
        "zone maps must not be resident after open (they are cacheable)"
    );

    // (b) fetched lazily via the metadata cache; decodes to one map per block,
    //     and each map is the block's first key (our builder).
    let section = reader.zone_maps().unwrap();
    let blob: &[u8] = match &*section {
        turba_engine::cache::MetaSection::ZoneMaps(v) => v.as_slice(),
        _ => panic!("expected a ZoneMaps section"),
    };
    assert!(!blob.is_empty(), "lazy fetch must return the zone-map blob");
    let maps = decode_zone_maps(blob);
    assert_eq!(
        maps.len(),
        meta.block_count as usize,
        "one zone map per block"
    );
    assert_eq!(maps[0], entries[0].key.as_slice());

    // (c) a second fetch hits the metadata cache (same Arc, no reload).
    let section2 = reader.zone_maps().unwrap();
    assert!(
        Arc::ptr_eq(&section, &section2),
        "second fetch must hit the metadata cache"
    );

    // (d) point reads still work with zone maps non-resident.
    let got = reader.get(&entries[1234].key, u64::MAX).unwrap().unwrap();
    assert_eq!(got.value, entries[1234].value);
}

/// Bloom is no longer resident: after open it is fetched + parsed lazily through
/// the metadata cache, and point lookups (present and absent keys) stay correct.
#[test]
fn bloom_is_cacheable_not_resident() {
    let entries = make_entries(1000);
    let (reader, _dir) = write_and_open(&entries, SSTableConfig::default());

    // Bloom is not resident in the reader (size accounting reads 0).
    assert_eq!(
        reader.bloom_bytes(),
        0,
        "bloom must not be resident after open (it is cacheable)"
    );

    // Present keys are found (bloom parsed + cached on first probe).
    for e in entries.iter().step_by(97) {
        let got = reader.get(&e.key, u64::MAX).unwrap();
        assert_eq!(got.map(|r| r.value), Some(e.value.clone()));
    }
    // Absent keys are correctly reported absent (bloom skip still works).
    assert!(reader.get(b"key_99999999", u64::MAX).unwrap().is_none());
    assert!(reader.get(b"absent", u64::MAX).unwrap().is_none());
}

/// Index is no longer resident: after open it is decoded + cached lazily, and
/// point reads + a full scan stay correct.
#[test]
fn index_is_cacheable_not_resident() {
    let entries = make_entries(1000);
    let (reader, _dir) = write_and_open(&entries, SSTableConfig::default());

    assert_eq!(
        reader.index_bytes(),
        0,
        "index must not be resident after open (it is cacheable)"
    );

    // Point reads work (index decoded + cached on first use).
    for e in entries.iter().step_by(53) {
        assert_eq!(
            reader.get(&e.key, u64::MAX).unwrap().map(|r| r.value),
            Some(e.value.clone())
        );
    }
    // A full scan reproduces every entry, in order.
    let scanned = reader.scan().unwrap();
    assert_eq!(scanned.len(), entries.len());
    for (got, want) in scanned.iter().zip(entries.iter()) {
        assert_eq!(got.key, want.key);
    }
}
