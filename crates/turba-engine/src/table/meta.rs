//! SSTable metadata: stored in the meta block and footer.
//!
//! Footer v2 (36 bytes, at end of file — what new SSTables write):
//! ```text
//! [magic: "XZT2" (4 bytes)]
//! [index_offset: u64 LE]
//! [bloom_offset: u64 LE]
//! [meta_offset: u64 LE]
//! [checksum: u64 LE]   // xxh3_64 of the 28-byte head (magic + 3 offsets)
//! ```
//! The checksum catches bit-rot in the offsets — without it a corrupted
//! offset silently mis-locates the index/bloom/meta block at open.
//!
//! Footer v1 (28 bytes, legacy "XYZT", no checksum) is still read so that a
//! data directory upgraded across the 0.8 format break — where some SSTables
//! predate v2 until they are recompacted — keeps opening.

// SPDX-License-Identifier: BUSL-1.1
use crate::error::{Error, Result};
use byteorder_lite::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::Write;

/// Decode per-block zone maps from the opaque blob in tag 12.
/// Returns a Vec of opaque byte slices, one per block.
pub fn decode_zone_maps(data: &[u8]) -> Vec<&[u8]> {
    if data.len() < 4 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut result = Vec::with_capacity(count);
    let mut pos = 4;
    for _ in 0..count {
        if pos + 2 > data.len() {
            break;
        }
        let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            break;
        }
        result.push(&data[pos..pos + len]);
        pos += len;
    }
    result
}

pub const FOOTER_MAGIC: &[u8; 4] = b"XYZT";
pub const FOOTER_SIZE: usize = 28; // 4 + 8 + 8 + 8
pub const FORMAT_VERSION: u8 = 1;

/// v2 footer magic: a checksummed footer (3f-meta). Distinct from
/// [`FOOTER_MAGIC`] so the reader can tell the two layouts apart from the file
/// tail alone. New SSTables always write v2.
pub const FOOTER_MAGIC_V2: &[u8; 4] = b"XZT2";
/// v2 footer size: 4 magic + 3×8 offsets + 8 checksum.
pub const FOOTER_SIZE_V2: usize = 36;

#[derive(Debug, Clone)]
pub struct Footer {
    pub index_offset: u64,
    pub bloom_offset: u64,
    pub meta_offset: u64,
}

impl Footer {
    /// Encode the v2 (checksummed) footer: magic + 3 offsets + xxh3_64 of that
    /// 28-byte head.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut payload = Vec::with_capacity(FOOTER_SIZE_V2);
        payload.write_all(FOOTER_MAGIC_V2)?;
        payload.write_u64::<LittleEndian>(self.index_offset)?;
        payload.write_u64::<LittleEndian>(self.bloom_offset)?;
        payload.write_u64::<LittleEndian>(self.meta_offset)?;
        let checksum = xxhash_rust::xxh3::xxh3_64(&payload);
        payload.write_u64::<LittleEndian>(checksum)?;
        w.write_all(&payload)?;
        Ok(())
    }

    /// Decode the footer from the file tail.
    ///
    /// `tail` must be the last `min(FOOTER_SIZE_V2, file_len)` bytes of the
    /// file. Returns the footer and its on-disk byte length — `FOOTER_SIZE_V2`
    /// for the checksummed v2 footer, `FOOTER_SIZE` for a legacy v1 footer — so
    /// the caller can bound the meta block (it ends `footer_size` bytes before
    /// EOF).
    ///
    /// # Errors
    /// - [`Error::ChecksumMismatch`] if a v2 footer's checksum does not match.
    /// - [`Error::InvalidMagic`] if neither a v2 nor a v1 magic is present.
    pub fn decode(tail: &[u8]) -> Result<(Self, usize)> {
        // v2 (checksummed): magic at the head of a FOOTER_SIZE_V2 window.
        if tail.len() >= FOOTER_SIZE_V2 {
            let w = &tail[tail.len() - FOOTER_SIZE_V2..];
            if &w[0..4] == FOOTER_MAGIC_V2 {
                let mut cs = &w[28..36];
                let stored = cs.read_u64::<LittleEndian>()?;
                let computed = xxhash_rust::xxh3::xxh3_64(&w[0..28]);
                if stored != computed {
                    return Err(Error::ChecksumMismatch);
                }
                let mut c = &w[4..28];
                let index_offset = c.read_u64::<LittleEndian>()?;
                let bloom_offset = c.read_u64::<LittleEndian>()?;
                let meta_offset = c.read_u64::<LittleEndian>()?;
                return Ok((
                    Self {
                        index_offset,
                        bloom_offset,
                        meta_offset,
                    },
                    FOOTER_SIZE_V2,
                ));
            }
        }
        // v1 (legacy, pre-3f-meta): magic at the head of a FOOTER_SIZE window.
        if tail.len() >= FOOTER_SIZE {
            let w = &tail[tail.len() - FOOTER_SIZE..];
            if &w[0..4] == FOOTER_MAGIC {
                let mut c = &w[4..28];
                let index_offset = c.read_u64::<LittleEndian>()?;
                let bloom_offset = c.read_u64::<LittleEndian>()?;
                let meta_offset = c.read_u64::<LittleEndian>()?;
                return Ok((
                    Self {
                        index_offset,
                        bloom_offset,
                        meta_offset,
                    },
                    FOOTER_SIZE,
                ));
            }
        }
        Err(Error::InvalidMagic)
    }
}

/// Metadata about an SSTable, persisted in the meta block.
#[derive(Debug, Clone)]
pub struct SSTableMeta {
    pub table_id: u64,
    pub block_count: u32,
    pub item_count: u64,
    pub key_min: Vec<u8>,
    pub key_max: Vec<u8>,
    pub seqno_min: u64,
    pub seqno_max: u64,
    pub compression: u8,
    pub format_version: u8,
    pub file_size: u64,
    pub tombstone_count: u64,
    /// Opaque zone map data for block-level data skipping.
    /// Computed and interpreted by higher layers (xyzdb-engine).
    /// Empty for SSTables without zone maps.
    pub zone_maps: Vec<u8>,
}

impl SSTableMeta {
    /// Encode as a sequence of tagged fields.
    ///
    /// Tags 1–11 use `[tag: u8][len: u16 LE][data]` — all values are
    /// fixed-size (scalars) or short (keys ≤ ~1 KB), well within u16.
    ///
    /// Tags 4 (key_min), 5 (key_max), and 12 (zone_maps) use
    /// `[tag: u8][len: u32 LE][data]`. These are the three variable-length
    /// fields in the meta block; scaling user keys (dictionary anchor
    /// values, custom keyspace keys) or compacted zone maps (observed
    /// ≈ 2 MB on a 64 MB SSTable with 2 040 blocks) past 65 535 bytes
    /// would silently truncate under the earlier u16 length, desynchronizing
    /// the decoder — v0.2.0-alpha Finding 4. The MANIFEST_VERSION 2 → 3
    /// bump ensures no v0.2.0-alpha data directory is ever parsed with
    /// the new layout.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);

        fn write_u16_field(buf: &mut Vec<u8>, tag: u8, data: &[u8]) {
            buf.push(tag);
            buf.write_u16::<LittleEndian>(data.len() as u16).unwrap();
            buf.extend_from_slice(data);
        }

        fn write_u32_field(buf: &mut Vec<u8>, tag: u8, data: &[u8]) {
            buf.push(tag);
            buf.write_u32::<LittleEndian>(data.len() as u32).unwrap();
            buf.extend_from_slice(data);
        }

        write_u16_field(&mut buf, 1, &self.table_id.to_le_bytes());
        write_u16_field(&mut buf, 2, &self.block_count.to_le_bytes());
        write_u16_field(&mut buf, 3, &self.item_count.to_le_bytes());
        write_u32_field(&mut buf, 4, &self.key_min);
        write_u32_field(&mut buf, 5, &self.key_max);
        write_u16_field(&mut buf, 6, &self.seqno_min.to_le_bytes());
        write_u16_field(&mut buf, 7, &self.seqno_max.to_le_bytes());
        write_u16_field(&mut buf, 8, &[self.compression]);
        write_u16_field(&mut buf, 9, &[self.format_version]);
        write_u16_field(&mut buf, 10, &self.file_size.to_le_bytes());
        write_u16_field(&mut buf, 11, &self.tombstone_count.to_le_bytes());
        if !self.zone_maps.is_empty() {
            write_u32_field(&mut buf, 12, &self.zone_maps);
        }
        buf
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut meta = SSTableMeta {
            table_id: 0,
            block_count: 0,
            item_count: 0,
            key_min: Vec::new(),
            key_max: Vec::new(),
            seqno_min: 0,
            seqno_max: 0,
            compression: 0,
            format_version: 0,
            file_size: 0,
            tombstone_count: 0,
            zone_maps: Vec::new(),
        };

        let mut pos = 0;
        while pos < data.len() {
            if pos + 1 > data.len() {
                break;
            }
            let tag = data[pos];
            pos += 1;

            // Length width is u32 for the three variable-length tags
            // (4 key_min, 5 key_max, 12 zone_maps) and u16 for every
            // fixed-size tag. See `encode` above and the v0.2.0-alpha
            // Finding 4 investigation + v0.2.1 audit for why.
            let len = if tag == 4 || tag == 5 || tag == 12 {
                if pos + 4 > data.len() {
                    break;
                }
                let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
                pos += 4;
                l
            } else {
                if pos + 2 > data.len() {
                    break;
                }
                let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                l
            };

            if pos + len > data.len() {
                return Err(Error::Corruption("meta field extends past data".into()));
            }
            let field = &data[pos..pos + len];
            pos += len;

            match tag {
                1 => {
                    meta.table_id = u64::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad table_id".into()))?,
                    )
                }
                2 => {
                    meta.block_count = u32::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad block_count".into()))?,
                    )
                }
                3 => {
                    meta.item_count = u64::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad item_count".into()))?,
                    )
                }
                4 => meta.key_min = field.to_vec(),
                5 => meta.key_max = field.to_vec(),
                6 => {
                    meta.seqno_min = u64::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad seqno_min".into()))?,
                    )
                }
                7 => {
                    meta.seqno_max = u64::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad seqno_max".into()))?,
                    )
                }
                8 => meta.compression = field[0],
                9 => meta.format_version = field[0],
                10 => {
                    meta.file_size = u64::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad file_size".into()))?,
                    )
                }
                11 => {
                    meta.tombstone_count = u64::from_le_bytes(
                        field
                            .try_into()
                            .map_err(|_| Error::Corruption("bad tombstone_count".into()))?,
                    )
                }
                12 => meta.zone_maps = field.to_vec(),
                _ => {} // skip unknown tags for forward compat
            }
        }

        Ok(meta)
    }
}

#[cfg(test)]
mod footer_tests {
    use super::*;

    fn enc(f: &Footer) -> Vec<u8> {
        let mut buf = Vec::new();
        f.encode(&mut buf).unwrap();
        buf
    }

    #[test]
    fn v2_roundtrip_preserves_offsets() {
        let f = Footer {
            index_offset: 100,
            bloom_offset: 250,
            meta_offset: 480,
        };
        let bytes = enc(&f);
        assert_eq!(bytes.len(), FOOTER_SIZE_V2);
        assert_eq!(&bytes[0..4], FOOTER_MAGIC_V2);
        let (got, size) = Footer::decode(&bytes).unwrap();
        assert_eq!(size, FOOTER_SIZE_V2);
        assert_eq!(got.index_offset, 100);
        assert_eq!(got.bloom_offset, 250);
        assert_eq!(got.meta_offset, 480);
    }

    #[test]
    fn v2_checksum_catches_corrupted_offset() {
        let f = Footer {
            index_offset: 1,
            bloom_offset: 2,
            meta_offset: 3,
        };
        let mut bytes = enc(&f);
        // Flip a bit inside an offset — exactly the silent mis-read the
        // checksum exists to catch.
        bytes[10] ^= 0xFF;
        assert!(matches!(
            Footer::decode(&bytes),
            Err(Error::ChecksumMismatch)
        ));
    }

    #[test]
    fn reads_legacy_v1_footer() {
        // A pre-3f-meta v1 footer: magic + 3 offsets, no checksum.
        let mut v1 = Vec::new();
        v1.write_all(FOOTER_MAGIC).unwrap();
        v1.write_u64::<LittleEndian>(11).unwrap();
        v1.write_u64::<LittleEndian>(22).unwrap();
        v1.write_u64::<LittleEndian>(33).unwrap();
        assert_eq!(v1.len(), FOOTER_SIZE);

        // The reader hands decode up to FOOTER_SIZE_V2 bytes from the end; for
        // a v1 file the leading bytes are meta-block tail, the v1 footer is at
        // the end.
        let mut tail = vec![0xAB; FOOTER_SIZE_V2 - FOOTER_SIZE];
        tail.extend_from_slice(&v1);
        let (got, size) = Footer::decode(&tail).unwrap();
        assert_eq!(size, FOOTER_SIZE);
        assert_eq!(got.index_offset, 11);
        assert_eq!(got.bloom_offset, 22);
        assert_eq!(got.meta_offset, 33);

        // And a tiny legacy file that only yields exactly 28 bytes.
        let (got2, size2) = Footer::decode(&v1).unwrap();
        assert_eq!(size2, FOOTER_SIZE);
        assert_eq!(got2.meta_offset, 33);
    }

    #[test]
    fn rejects_garbage_tail() {
        let junk = vec![0u8; FOOTER_SIZE_V2];
        assert!(matches!(Footer::decode(&junk), Err(Error::InvalidMagic)));
    }
}
