//! Block format: the atomic unit of storage in turba-engine.
//!
//! On-disk layout (34 byte header + compressed data):
//! ```text
//! [magic: 4 bytes = "XYZB"]
//! [block_type: u8]              0=Data, 1=Index
//! [compression_type: u8]        0=None, 1=LZ4, 2=Zstd
//! [checksum: 16 bytes XXH3-128 of compressed data]
//! [data_length: u32 LE]         compressed size
//! [uncompressed_length: u32 LE]
//! [header_checksum: u32 LE]     XXH3-32 of first 30 bytes
//! --- 34 bytes total ---
//! [compressed_data: data_length bytes]
//! ```
//!
//! Entry format inside block (after decompression):
//! ```text
//! Entries are stored with prefix truncation relative to previous entry.
//! At restart points (every N entries), the full key is stored.
//!
//! [entry at restart point]
//!   shared_prefix_len: varint = 0 (full key)
//!   suffix_len: varint
//!   suffix: [u8; suffix_len]
//!   value_type: u8
//!   seqno: varint(u64)
//!   value_len: varint(u32)
//!   value: [u8; value_len]     (omitted if tombstone)
//!
//! [entry between restart points]
//!   shared_prefix_len: varint   (bytes shared with previous key)
//!   suffix_len: varint
//!   suffix: [u8; suffix_len]   (remaining bytes after shared prefix)
//!   value_type: u8
//!   seqno: varint(u64)
//!   value_len: varint(u32)
//!   value: [u8; value_len]     (omitted if tombstone)
//!
//! [restart section at end]
//!   restart_point_count: u32 LE
//!   offsets: [u32 LE × count]  (byte offset of each restart entry)
//! ```

// SPDX-License-Identifier: BUSL-1.1
use crate::compression::{self, CompressionType};
use crate::error::{Error, Result};
use crate::types::{Entry, SeqNo, ValueType};
use byteorder_lite::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::Cursor;

const BLOCK_MAGIC: &[u8; 4] = b"XYZB";
const HEADER_SIZE: usize = 34; // 4+1+1+16+4+4+4
const DEFAULT_RESTART_INTERVAL: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockType {
    Data = 0,
    Index = 1,
}

impl BlockType {
    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Data),
            1 => Some(Self::Index),
            _ => None,
        }
    }
}

// --- Varint helpers ---

fn encode_varint(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn decode_varint(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return Err(Error::InvalidEntry("unexpected end of varint".into()));
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::InvalidEntry("varint too large".into()));
        }
    }
}

// --- Encoding ---

/// Encode entries into a block with prefix truncation and restart points.
fn encode_entries(entries: &[Entry], restart_interval: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(entries.len() * 64);
    let mut restart_offsets = Vec::new();
    let mut prev_key: &[u8] = &[];

    for (i, entry) in entries.iter().enumerate() {
        let is_restart = i % restart_interval == 0;
        if is_restart {
            restart_offsets.push(buf.len() as u32);
        }

        let shared = if is_restart {
            0
        } else {
            common_prefix_len(prev_key, &entry.key)
        };
        let suffix = &entry.key[shared..];

        encode_varint(&mut buf, shared as u64);
        encode_varint(&mut buf, suffix.len() as u64);
        buf.extend_from_slice(suffix);
        buf.push(entry.value_type as u8);
        encode_varint(&mut buf, entry.seqno);
        if entry.value_type == ValueType::Value {
            encode_varint(&mut buf, entry.value.len() as u64);
            buf.extend_from_slice(&entry.value);
        } else {
            encode_varint(&mut buf, 0);
        }

        prev_key = &entry.key;
    }

    // Restart section
    let count = restart_offsets.len() as u32;
    for offset in &restart_offsets {
        buf.write_u32::<LittleEndian>(*offset).unwrap();
    }
    buf.write_u32::<LittleEndian>(count).unwrap();

    buf
}

/// Decode entries from raw (decompressed) block data.
fn decode_entries(data: &[u8]) -> Result<Vec<Entry>> {
    if data.len() < 4 {
        return Err(Error::InvalidEntry(
            "block too small for restart count".into(),
        ));
    }

    // Read restart count from last 4 bytes
    let restart_count = {
        let mut c = Cursor::new(&data[data.len() - 4..]);
        c.read_u32::<LittleEndian>()? as usize
    };

    // Restart offsets are before the count
    let restart_section_size = restart_count * 4 + 4;
    if data.len() < restart_section_size {
        return Err(Error::InvalidEntry(
            "block too small for restart section".into(),
        ));
    }
    let entries_end = data.len() - restart_section_size;

    let mut entries = Vec::new();
    let mut pos = 0;
    let mut prev_key = Vec::new();

    while pos < entries_end {
        let shared = decode_varint(data, &mut pos)? as usize;
        let suffix_len = decode_varint(data, &mut pos)? as usize;

        if pos + suffix_len > entries_end {
            return Err(Error::InvalidEntry(
                "suffix extends past entries section".into(),
            ));
        }
        let suffix = &data[pos..pos + suffix_len];
        pos += suffix_len;

        // Reconstruct full key
        let mut key = Vec::with_capacity(shared + suffix_len);
        if shared > prev_key.len() {
            return Err(Error::InvalidEntry(
                "shared prefix exceeds previous key".into(),
            ));
        }
        key.extend_from_slice(&prev_key[..shared]);
        key.extend_from_slice(suffix);

        if pos >= entries_end {
            return Err(Error::InvalidEntry(
                "unexpected end reading value_type".into(),
            ));
        }
        let vtype_byte = data[pos];
        pos += 1;
        let value_type = ValueType::from_u8(vtype_byte)
            .ok_or_else(|| Error::InvalidEntry(format!("unknown value_type {vtype_byte}")))?;

        let seqno = decode_varint(data, &mut pos)? as SeqNo;
        let value_len = decode_varint(data, &mut pos)? as usize;

        let value = if value_type == ValueType::Value {
            if pos + value_len > entries_end {
                return Err(Error::InvalidEntry(
                    "value extends past entries section".into(),
                ));
            }
            let v = data[pos..pos + value_len].to_vec();
            pos += value_len;
            v
        } else {
            Vec::new()
        };

        prev_key = key.clone();
        entries.push(Entry::new(key, value, seqno, value_type));
    }

    Ok(entries)
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

// --- Block header ---

fn xxh3_128(data: &[u8]) -> u128 {
    xxhash_rust::xxh3::xxh3_128(data)
}

fn xxh3_32(data: &[u8]) -> u32 {
    xxhash_rust::xxh3::xxh3_64(data) as u32
}

fn encode_header(
    block_type: BlockType,
    compression: CompressionType,
    compressed_data: &[u8],
    uncompressed_len: usize,
) -> [u8; HEADER_SIZE] {
    let mut header = [0u8; HEADER_SIZE];
    header[0..4].copy_from_slice(BLOCK_MAGIC);
    header[4] = block_type as u8;
    header[5] = compression.to_u8();

    let checksum = xxh3_128(compressed_data);
    header[6..22].copy_from_slice(&checksum.to_le_bytes());

    let data_len = compressed_data.len() as u32;
    header[22..26].copy_from_slice(&data_len.to_le_bytes());

    let uncomp_len = uncompressed_len as u32;
    header[26..30].copy_from_slice(&uncomp_len.to_le_bytes());

    // Header checksum covers first 30 bytes
    let hdr_checksum = xxh3_32(&header[..30]);
    header[30..34].copy_from_slice(&hdr_checksum.to_le_bytes());

    header
}

struct DecodedHeader {
    #[allow(dead_code)] // used in later phases for type validation
    block_type: BlockType,
    compression: CompressionType,
    data_checksum: u128,
    data_length: u32,
    uncompressed_length: u32,
}

fn decode_header(raw: &[u8]) -> Result<DecodedHeader> {
    if raw.len() < HEADER_SIZE {
        return Err(Error::InvalidHeader);
    }

    if &raw[0..4] != BLOCK_MAGIC {
        return Err(Error::InvalidMagic);
    }

    // Validate header checksum first
    let stored_hdr_checksum = u32::from_le_bytes([raw[30], raw[31], raw[32], raw[33]]);
    let computed_hdr_checksum = xxh3_32(&raw[..30]);
    if stored_hdr_checksum != computed_hdr_checksum {
        return Err(Error::ChecksumMismatch);
    }

    let block_type = BlockType::from_u8(raw[4]).ok_or(Error::InvalidHeader)?;
    let compression = CompressionType::from_u8(raw[5]).ok_or(Error::InvalidHeader)?;
    let data_checksum = u128::from_le_bytes(raw[6..22].try_into().unwrap());
    let data_length = u32::from_le_bytes(raw[22..26].try_into().unwrap());
    let uncompressed_length = u32::from_le_bytes(raw[26..30].try_into().unwrap());

    Ok(DecodedHeader {
        block_type,
        compression,
        data_checksum,
        data_length,
        uncompressed_length,
    })
}

// --- Public API ---

/// Encode entries into a compressed block with header and checksums.
pub fn encode(entries: &[Entry], compression: CompressionType, block_type: BlockType) -> Vec<u8> {
    encode_with_restart_interval(entries, compression, block_type, DEFAULT_RESTART_INTERVAL)
}

pub fn encode_with_restart_interval(
    entries: &[Entry],
    compression: CompressionType,
    block_type: BlockType,
    restart_interval: usize,
) -> Vec<u8> {
    let raw = encode_entries(entries, restart_interval);
    let compressed = compression::compress(&raw, compression, None);
    let header = encode_header(block_type, compression, &compressed, raw.len());

    let mut out = Vec::with_capacity(HEADER_SIZE + compressed.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&compressed);
    out
}

/// Validate block checksums without decompressing.
pub fn validate_checksum(raw: &[u8]) -> Result<()> {
    let hdr = decode_header(raw)?;
    let data_start = HEADER_SIZE;
    let data_end = data_start + hdr.data_length as usize;
    if raw.len() < data_end {
        return Err(Error::Corruption("block truncated".into()));
    }
    let actual = xxh3_128(&raw[data_start..data_end]);
    if actual != hdr.data_checksum {
        return Err(Error::ChecksumMismatch);
    }
    Ok(())
}

/// Decode a block: validate checksums, decompress, parse entries.
pub fn decode(raw: &[u8]) -> Result<Vec<Entry>> {
    let hdr = decode_header(raw)?;
    let data_start = HEADER_SIZE;
    let data_end = data_start + hdr.data_length as usize;
    if raw.len() < data_end {
        return Err(Error::Corruption("block truncated".into()));
    }

    let compressed_data = &raw[data_start..data_end];

    // Validate data checksum
    let actual = xxh3_128(compressed_data);
    if actual != hdr.data_checksum {
        return Err(Error::ChecksumMismatch);
    }

    let decompressed = compression::decompress(
        compressed_data,
        hdr.compression,
        hdr.uncompressed_length as usize,
        None,
    )?;
    // 4b: the data checksum covers the COMPRESSED bytes, so a decompression
    // bug — or a tampered `uncompressed_length` — that yields a wrong-sized
    // output would otherwise pass undetected and feed truncated/garbage data
    // to the parser. Cross-check the decompressed length against the header.
    // Runtime error in release (NOT `debug_assert`) — this guards on-disk
    // integrity, which must hold in production builds.
    if decompressed.len() != hdr.uncompressed_length as usize {
        return Err(Error::Corruption(format!(
            "decompressed length {} != header uncompressed_length {}",
            decompressed.len(),
            hdr.uncompressed_length
        )));
    }
    decode_entries(&decompressed)
}

/// Binary search for a specific key within a decoded block's entries.
/// Returns the entry with the highest seqno <= visible_seqno for the given user_key.
pub fn point_read(entries: &[Entry], user_key: &[u8], visible_seqno: SeqNo) -> Option<Entry> {
    // Entries are sorted by (user_key ASC, seqno DESC).
    // Find the first entry with this user_key, then scan for visible seqno.
    let idx = entries.partition_point(|e| e.key.as_slice() < user_key);

    for entry in &entries[idx..] {
        if entry.key != user_key {
            break;
        }
        if entry.seqno <= visible_seqno {
            return Some(entry.clone());
        }
    }
    None
}

/// The size of the block header in bytes.
pub const fn header_size() -> usize {
    HEADER_SIZE
}

#[cfg(test)]
mod fsyncgate_4b_tests {
    use super::*;

    /// 4b regression: a block whose decompressed length disagrees with the
    /// header's `uncompressed_length` must be rejected — the data checksum
    /// only covers the compressed bytes, so this is the only line of defence
    /// against a wrong-sized decompression feeding garbage to the parser.
    #[test]
    fn decode_rejects_mismatched_uncompressed_length() {
        let entries = vec![Entry::new(
            b"k".to_vec(),
            b"v".to_vec(),
            1,
            ValueType::Value,
        )];
        let mut raw = encode(&entries, CompressionType::None, BlockType::Data);
        assert!(decode(&raw).is_ok(), "control: a clean block decodes");

        // Tamper the header's uncompressed_length (bytes 26..30) to a wrong
        // value, then recompute the header checksum (bytes 30..34) so header
        // validation still passes and we reach the new cross-check.
        let real = u32::from_le_bytes(raw[26..30].try_into().unwrap());
        raw[26..30].copy_from_slice(&real.wrapping_add(1).to_le_bytes());
        let hc = xxh3_32(&raw[..30]);
        raw[30..34].copy_from_slice(&hc.to_le_bytes());

        assert!(
            matches!(decode(&raw), Err(Error::Corruption(_))),
            "decode must reject a length mismatch instead of returning wrong data"
        );
    }
}
