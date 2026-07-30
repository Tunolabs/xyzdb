use turba_engine::block::{self, BlockType};
use turba_engine::compression::CompressionType;
use turba_engine::types::{Entry, ValueType};

fn sample_entries(count: usize) -> Vec<Entry> {
    (0..count)
        .map(|i| {
            Entry::new(
                format!("key_{i:06}").into_bytes(),
                format!("value_{i}").into_bytes(),
                (i + 1) as u64,
                ValueType::Value,
            )
        })
        .collect()
}

fn sample_entries_shared_prefix(count: usize) -> Vec<Entry> {
    (0..count)
        .map(|i| {
            Entry::new(
                format!("lobe_0001/entity_{i:06}").into_bytes(),
                format!("data_{i}").into_bytes(),
                (i + 1) as u64,
                ValueType::Value,
            )
        })
        .collect()
}

// --- Roundtrip tests ---

#[test]
fn block_roundtrip_no_compression() {
    let entries = sample_entries(100);
    let encoded = block::encode(&entries, CompressionType::None, BlockType::Data);
    let decoded = block::decode(&encoded).unwrap();
    assert_eq!(entries, decoded);
}

#[test]
fn block_roundtrip_lz4() {
    let entries = sample_entries(100);
    let encoded = block::encode(&entries, CompressionType::Lz4, BlockType::Data);
    let decoded = block::decode(&encoded).unwrap();
    assert_eq!(entries, decoded);
}

#[test]
fn block_roundtrip_zstd() {
    let entries = sample_entries(100);
    let encoded = block::encode(&entries, CompressionType::Zstd(3), BlockType::Data);
    let decoded = block::decode(&encoded).unwrap();
    assert_eq!(entries, decoded);
}

#[test]
fn block_roundtrip_single_entry() {
    let entries = vec![Entry::new(
        b"k".to_vec(),
        b"v".to_vec(),
        1,
        ValueType::Value,
    )];
    for ct in [
        CompressionType::None,
        CompressionType::Lz4,
        CompressionType::Zstd(1),
    ] {
        let encoded = block::encode(&entries, ct, BlockType::Data);
        let decoded = block::decode(&encoded).unwrap();
        assert_eq!(entries, decoded);
    }
}

#[test]
fn block_roundtrip_with_tombstones() {
    let entries = vec![
        Entry::new(b"key_a".to_vec(), b"val_a".to_vec(), 1, ValueType::Value),
        Entry::new(b"key_b".to_vec(), vec![], 2, ValueType::Tombstone),
        Entry::new(b"key_c".to_vec(), b"val_c".to_vec(), 3, ValueType::Value),
    ];
    let encoded = block::encode(&entries, CompressionType::Lz4, BlockType::Data);
    let decoded = block::decode(&encoded).unwrap();
    assert_eq!(entries, decoded);
    assert!(decoded[1].value.is_empty());
    assert_eq!(decoded[1].value_type, ValueType::Tombstone);
}

// --- Checksum corruption tests ---

#[test]
fn block_checksum_detects_data_corruption() {
    let entries = sample_entries(50);
    let mut encoded = block::encode(&entries, CompressionType::Lz4, BlockType::Data);

    // Flip a byte in the compressed data section (after 32-byte header)
    let corrupt_pos = block::header_size() + 5;
    encoded[corrupt_pos] ^= 0xFF;

    let result = block::decode(&encoded);
    assert!(result.is_err(), "should detect data corruption");

    let result = block::validate_checksum(&encoded);
    assert!(
        result.is_err(),
        "validate_checksum should detect corruption"
    );
}

#[test]
fn block_header_checksum_detects_corruption() {
    let entries = sample_entries(10);
    let mut encoded = block::encode(&entries, CompressionType::None, BlockType::Data);

    // Flip a byte in the header (not the header checksum itself)
    encoded[5] ^= 0xFF; // corruption_type byte

    let result = block::decode(&encoded);
    assert!(result.is_err(), "should detect header corruption");
}

#[test]
fn block_truncated_detected() {
    let entries = sample_entries(50);
    let encoded = block::encode(&entries, CompressionType::Lz4, BlockType::Data);

    // Truncate to just the header
    let truncated = &encoded[..block::header_size()];
    let result = block::decode(truncated);
    assert!(result.is_err(), "should detect truncated block");
}

// --- Prefix truncation ---

#[test]
fn block_prefix_truncation_reduces_size() {
    let entries = sample_entries_shared_prefix(200);

    // Encode with restart_interval=16 (prefix truncation active)
    let with_prefix =
        block::encode_with_restart_interval(&entries, CompressionType::None, BlockType::Data, 16);

    // Encode with restart_interval=1 (no prefix truncation — every entry is a restart)
    let without_prefix =
        block::encode_with_restart_interval(&entries, CompressionType::None, BlockType::Data, 1);

    assert!(
        with_prefix.len() < without_prefix.len(),
        "prefix truncation should reduce size: {} vs {}",
        with_prefix.len(),
        without_prefix.len()
    );

    // Verify roundtrip still works
    let decoded = block::decode(&with_prefix).unwrap();
    assert_eq!(entries, decoded);
}

// --- Binary search / point read ---

#[test]
fn block_point_read_binary_search() {
    let entries = sample_entries(1000);
    let encoded = block::encode(&entries, CompressionType::Lz4, BlockType::Data);
    let decoded = block::decode(&encoded).unwrap();

    // Find existing key
    let result = block::point_read(&decoded, b"key_000500", u64::MAX);
    assert!(result.is_some());
    let entry = result.unwrap();
    assert_eq!(entry.key, b"key_000500");
    assert_eq!(entry.seqno, 501);

    // Find first key
    let result = block::point_read(&decoded, b"key_000000", u64::MAX);
    assert!(result.is_some());

    // Find last key
    let result = block::point_read(&decoded, b"key_000999", u64::MAX);
    assert!(result.is_some());

    // Absent key
    let result = block::point_read(&decoded, b"key_999999", u64::MAX);
    assert!(result.is_none());
}

#[test]
fn block_point_read_mvcc_visibility() {
    // Same key, multiple versions (seqno DESC order)
    let entries = vec![
        Entry::new(b"key_a".to_vec(), b"v3".to_vec(), 30, ValueType::Value),
        Entry::new(b"key_a".to_vec(), b"v2".to_vec(), 20, ValueType::Value),
        Entry::new(b"key_a".to_vec(), b"v1".to_vec(), 10, ValueType::Value),
    ];

    // Visible seqno 25 → should see version at seqno 20
    let result = block::point_read(&entries, b"key_a", 25);
    assert!(result.is_some());
    assert_eq!(result.unwrap().value, b"v2");

    // Visible seqno 30 → should see version at seqno 30
    let result = block::point_read(&entries, b"key_a", 30);
    assert!(result.is_some());
    assert_eq!(result.unwrap().value, b"v3");

    // Visible seqno 5 → nothing visible
    let result = block::point_read(&entries, b"key_a", 5);
    assert!(result.is_none());
}

// --- Compression ratio comparison ---

#[test]
fn compression_lz4_vs_zstd_ratio() {
    // Simulate xyzDB-like data: keys with shared prefix, postcard-like values
    let entries: Vec<Entry> = (0..5000)
        .map(|i| {
            let key = format!("lobe_0001/entity_{i:08}").into_bytes();
            // Simulate postcard-serialized record with field IDs
            let mut value = Vec::with_capacity(200);
            value.extend_from_slice(&[0x00, 0x01]); // field ID prefix
            value.extend_from_slice(format!("Juan García #{i}").as_bytes());
            value.extend_from_slice(&[0x00, 0x02]);
            value.extend_from_slice(format!("RFC{i:010}").as_bytes());
            value.extend_from_slice(&[0x00, 0x03]);
            value.extend_from_slice(&(i as f64 * 1000.0).to_le_bytes());
            Entry::new(key, value, (i + 1) as u64, ValueType::Value)
        })
        .collect();

    let none_size = block::encode(&entries, CompressionType::None, BlockType::Data).len();
    let lz4_size = block::encode(&entries, CompressionType::Lz4, BlockType::Data).len();
    let zstd3_size = block::encode(&entries, CompressionType::Zstd(3), BlockType::Data).len();
    let zstd9_size = block::encode(&entries, CompressionType::Zstd(9), BlockType::Data).len();

    eprintln!("Block sizes for 5000 xyzDB-like entries:");
    eprintln!("  None:    {none_size:>8} bytes (100%)");
    eprintln!(
        "  LZ4:     {lz4_size:>8} bytes ({:.1}%)",
        lz4_size as f64 / none_size as f64 * 100.0
    );
    eprintln!(
        "  Zstd-3:  {zstd3_size:>8} bytes ({:.1}%)",
        zstd3_size as f64 / none_size as f64 * 100.0
    );
    eprintln!(
        "  Zstd-9:  {zstd9_size:>8} bytes ({:.1}%)",
        zstd9_size as f64 / none_size as f64 * 100.0
    );

    // Zstd should beat LZ4
    assert!(zstd3_size < lz4_size, "Zstd-3 should be smaller than LZ4");
    // Note: Zstd-9 is not always smaller than Zstd-3 for small blocks
    // (dictionary overhead can outweigh gains). Just verify it compresses.
    assert!(zstd9_size < none_size, "Zstd-9 should be smaller than none");
    // All compressed should be smaller than none
    assert!(lz4_size < none_size);
}

#[test]
fn compression_roundtrip_all_types() {
    let entries = sample_entries(200);
    for ct in [
        CompressionType::None,
        CompressionType::Lz4,
        CompressionType::Zstd(1),
        CompressionType::Zstd(3),
        CompressionType::Zstd(9),
    ] {
        let encoded = block::encode(&entries, ct, BlockType::Data);
        let decoded = block::decode(&encoded).unwrap();
        assert_eq!(entries, decoded, "roundtrip failed for {ct:?}");
    }
}

// --- Restart points enable seek ---

#[test]
fn block_restart_points_enable_binary_search() {
    // With restart_interval=4, every 4th entry stores the full key.
    // This means we can binary search restart points to narrow down.
    let entries = sample_entries(100);
    let encoded =
        block::encode_with_restart_interval(&entries, CompressionType::None, BlockType::Data, 4);
    let decoded = block::decode(&encoded).unwrap();

    // All entries should be correctly decoded despite short restart interval
    assert_eq!(entries, decoded);

    // Point reads should work
    for i in [0, 3, 4, 50, 99] {
        let key = format!("key_{i:06}");
        let result = block::point_read(&decoded, key.as_bytes(), u64::MAX);
        assert!(result.is_some(), "point_read failed for {key}");
    }
}
