//! Journal entry encoding/decoding.
//!
//! A batch in the WAL:
//! ```text
//! [Start tag=1, item_count: u32 LE, seqno: u64 LE]
//! [Item  tag=2, keyspace_id: u8, key_len: u32 LE, key, value_len: u32 LE, value, value_type: u8] × N
//! [End   tag=3, checksum: u128 LE (XXH3-128 of Start + all Items)]
//! ```
//!
//! If recovery finds Start without matching End (or checksum mismatch), the batch is discarded.

use crate::types::ValueType;
use byteorder_lite::{LittleEndian, WriteBytesExt};

const TAG_START: u8 = 1;
const TAG_ITEM: u8 = 2;
const TAG_END: u8 = 3;

/// One item in a write batch.
#[derive(Debug, Clone)]
pub struct BatchItem {
    pub keyspace_id: u8,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub value_type: ValueType,
}

/// Encode a complete batch (Start + Items + End with checksum) into bytes.
pub fn encode_batch(seqno: u64, items: &[BatchItem]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(items.len() * 128 + 32);

    // Start
    buf.push(TAG_START);
    buf.write_u32::<LittleEndian>(items.len() as u32).unwrap();
    buf.write_u64::<LittleEndian>(seqno).unwrap();

    // Items
    for item in items {
        buf.push(TAG_ITEM);
        buf.push(item.keyspace_id);
        buf.write_u32::<LittleEndian>(item.key.len() as u32)
            .unwrap();
        buf.extend_from_slice(&item.key);
        buf.write_u32::<LittleEndian>(item.value.len() as u32)
            .unwrap();
        buf.extend_from_slice(&item.value);
        buf.push(item.value_type as u8);
    }

    // End with checksum of everything so far
    let checksum = xxhash_rust::xxh3::xxh3_128(&buf);
    buf.push(TAG_END);
    buf.extend_from_slice(&checksum.to_le_bytes());

    buf
}

/// A recovered batch from the journal.
#[derive(Debug)]
pub struct RecoveredBatch {
    pub seqno: u64,
    pub items: Vec<BatchItem>,
}

/// Parse all valid batches from raw journal bytes.
/// Incomplete or checksum-invalid batches are discarded.
pub fn decode_batches(data: &[u8]) -> Vec<RecoveredBatch> {
    let mut batches = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        match parse_one_batch(data, &mut pos) {
            Some(batch) => batches.push(batch),
            None => break, // corrupted or incomplete — stop
        }
    }

    batches
}

fn parse_one_batch(data: &[u8], pos: &mut usize) -> Option<RecoveredBatch> {
    let start_pos = *pos;

    // Expect TAG_START
    if *pos >= data.len() || data[*pos] != TAG_START {
        return None;
    }
    *pos += 1;

    // item_count (u32) + seqno (u64) = 12 bytes
    if *pos + 12 > data.len() {
        return None;
    }
    let item_count = u32::from_le_bytes(data[*pos..*pos + 4].try_into().ok()?) as usize;
    *pos += 4;
    let seqno = u64::from_le_bytes(data[*pos..*pos + 8].try_into().ok()?);
    *pos += 8;

    // Parse items. Capacity is NOT pre-allocated from `item_count`: that field
    // comes from untrusted input (a corrupted or malicious WAL), and a bogus
    // value like u32::MAX would OOM the process before any bounds check runs.
    // Items are appended as they're parsed; per-item bounds checks below gate growth.
    let mut items: Vec<BatchItem> = Vec::new();
    for _ in 0..item_count {
        if *pos >= data.len() || data[*pos] != TAG_ITEM {
            return None;
        }
        *pos += 1;

        if *pos >= data.len() {
            return None;
        }
        let keyspace_id = data[*pos];
        *pos += 1;

        if *pos + 4 > data.len() {
            return None;
        }
        let key_len = u32::from_le_bytes(data[*pos..*pos + 4].try_into().ok()?) as usize;
        *pos += 4;

        if *pos + key_len > data.len() {
            return None;
        }
        let key = data[*pos..*pos + key_len].to_vec();
        *pos += key_len;

        if *pos + 4 > data.len() {
            return None;
        }
        let value_len = u32::from_le_bytes(data[*pos..*pos + 4].try_into().ok()?) as usize;
        *pos += 4;

        if *pos + value_len > data.len() {
            return None;
        }
        let value = data[*pos..*pos + value_len].to_vec();
        *pos += value_len;

        if *pos >= data.len() {
            return None;
        }
        let value_type = ValueType::from_u8(data[*pos])?;
        *pos += 1;

        items.push(BatchItem {
            keyspace_id,
            key,
            value,
            value_type,
        });
    }

    // Expect TAG_END + checksum
    if *pos >= data.len() || data[*pos] != TAG_END {
        return None;
    }
    *pos += 1;

    if *pos + 16 > data.len() {
        return None;
    }
    let stored_checksum = u128::from_le_bytes(data[*pos..*pos + 16].try_into().ok()?);
    *pos += 16;

    // Validate checksum: covers from start_pos to just before TAG_END
    let payload = &data[start_pos..*pos - 17]; // everything except TAG_END + checksum
    let computed = xxhash_rust::xxh3::xxh3_128(payload);
    if stored_checksum != computed {
        return None; // checksum mismatch — discard
    }

    Some(RecoveredBatch { seqno, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let items = vec![
            BatchItem {
                keyspace_id: 0,
                key: b"k1".to_vec(),
                value: b"v1".to_vec(),
                value_type: ValueType::Value,
            },
            BatchItem {
                keyspace_id: 1,
                key: b"k2".to_vec(),
                value: b"v2".to_vec(),
                value_type: ValueType::Value,
            },
            BatchItem {
                keyspace_id: 2,
                key: b"k3".to_vec(),
                value: vec![],
                value_type: ValueType::Tombstone,
            },
        ];
        let encoded = encode_batch(42, &items);
        let batches = decode_batches(&encoded);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].seqno, 42);
        assert_eq!(batches[0].items.len(), 3);
        assert_eq!(batches[0].items[2].value_type, ValueType::Tombstone);
    }

    // Regression: a crafted batch header with item_count = u32::MAX used to
    // trigger `Vec::with_capacity(u32::MAX)` and OOM the process before any
    // bounds check ran. `parse_one_batch` now grows the vec incrementally.
    #[test]
    fn malicious_item_count_does_not_oom() {
        let bogus = [
            0x01, // TAG_START
            0xFF, 0xFF, 0xFF, 0xFF, // item_count = u32::MAX
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // seqno
            0xFF, 0xFF, 0xFF, // garbage, not a valid item
        ];
        let batches = decode_batches(&bogus);
        assert!(batches.is_empty());
    }

    #[test]
    fn truncated_batch_discarded() {
        let items = vec![BatchItem {
            keyspace_id: 0,
            key: b"k".to_vec(),
            value: b"v".to_vec(),
            value_type: ValueType::Value,
        }];
        let mut encoded = encode_batch(1, &items);
        encoded.truncate(encoded.len() / 2); // truncate mid-batch
        let batches = decode_batches(&encoded);
        assert!(batches.is_empty());
    }

    #[test]
    fn corrupted_checksum_discarded() {
        let items = vec![BatchItem {
            keyspace_id: 0,
            key: b"k".to_vec(),
            value: b"v".to_vec(),
            value_type: ValueType::Value,
        }];
        let mut encoded = encode_batch(1, &items);
        // Corrupt a byte in the payload
        encoded[5] ^= 0xFF;
        let batches = decode_batches(&encoded);
        assert!(batches.is_empty());
    }

    #[test]
    fn multiple_batches() {
        let b1 = encode_batch(
            1,
            &[BatchItem {
                keyspace_id: 0,
                key: b"a".to_vec(),
                value: b"1".to_vec(),
                value_type: ValueType::Value,
            }],
        );
        let b2 = encode_batch(
            2,
            &[BatchItem {
                keyspace_id: 0,
                key: b"b".to_vec(),
                value: b"2".to_vec(),
                value_type: ValueType::Value,
            }],
        );
        let mut combined = b1;
        combined.extend(b2);
        let batches = decode_batches(&combined);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].seqno, 1);
        assert_eq!(batches[1].seqno, 2);
    }
}
