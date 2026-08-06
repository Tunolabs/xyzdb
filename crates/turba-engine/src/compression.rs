// SPDX-License-Identifier: BUSL-1.1
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None,
    Lz4,
    Zstd(i32), // compression level 1-22
    /// Zstd with a trained dictionary (byte 3 on disk).
    /// The dictionary bytes are passed separately to compress/decompress.
    ZstdDict(i32),
}

impl CompressionType {
    pub fn to_u8(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Lz4 => 1,
            Self::Zstd(_) => 2,
            Self::ZstdDict(_) => 3,
        }
    }

    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::None),
            1 => Some(Self::Lz4),
            2 => Some(Self::Zstd(3)),
            3 => Some(Self::ZstdDict(3)), // dict loaded from tree config
            _ => None,
        }
    }
}

/// Compress data. For ZstdDict, `dict` must be Some.
pub fn compress(data: &[u8], ct: CompressionType, dict: Option<&[u8]>) -> Vec<u8> {
    match ct {
        CompressionType::None => data.to_vec(),
        CompressionType::Lz4 => lz4_flex::compress_prepend_size(data),
        CompressionType::Zstd(level) => zstd::bulk::compress(data, level).expect("zstd compress"),
        CompressionType::ZstdDict(level) => {
            let d = dict.expect("ZstdDict requires dictionary bytes");
            let mut compressor =
                zstd::bulk::Compressor::with_dictionary(level, d).expect("valid zstd dictionary");
            compressor.compress(data).expect("zstd dict compress")
        }
    }
}

/// Decompress data. For ZstdDict (type byte 3), `dict` must be Some.
pub fn decompress(
    data: &[u8],
    ct: CompressionType,
    uncompressed_len: usize,
    dict: Option<&[u8]>,
) -> Result<Vec<u8>> {
    match ct {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Lz4 => lz4_flex::decompress_size_prepended(data)
            .map_err(|e| Error::Decompress(format!("LZ4: {e}"))),
        CompressionType::Zstd(_) => zstd::bulk::decompress(data, uncompressed_len)
            .map_err(|e| Error::Decompress(format!("Zstd: {e}"))),
        CompressionType::ZstdDict(_) => {
            let d = dict.ok_or_else(|| Error::Decompress("ZstdDict requires dictionary".into()))?;
            let mut decompressor = zstd::bulk::Decompressor::with_dictionary(d)
                .map_err(|e| Error::Decompress(format!("Zstd dict init: {e}")))?;
            decompressor
                .decompress(data, uncompressed_len)
                .map_err(|e| Error::Decompress(format!("Zstd dict: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_none() {
        let data = b"hello world";
        let compressed = compress(data, CompressionType::None, None);
        let decompressed =
            decompress(&compressed, CompressionType::None, data.len(), None).unwrap();
        assert_eq!(data.as_slice(), &decompressed);
    }

    #[test]
    fn roundtrip_lz4() {
        let data = b"hello world hello world hello world";
        let compressed = compress(data, CompressionType::Lz4, None);
        let decompressed = decompress(&compressed, CompressionType::Lz4, data.len(), None).unwrap();
        assert_eq!(data.as_slice(), &decompressed);
    }

    #[test]
    fn roundtrip_zstd_levels() {
        let data = vec![42u8; 10_000];
        for level in [1, 3, 9, 19] {
            let ct = CompressionType::Zstd(level);
            let compressed = compress(&data, ct, None);
            let decompressed = decompress(&compressed, ct, data.len(), None).unwrap();
            assert_eq!(data, decompressed, "failed at zstd level {level}");
        }
    }

    #[test]
    fn roundtrip_zstd_dict() {
        // Train a trivial dictionary from sample data
        let mut samples = Vec::new();
        for i in 0..100u32 {
            let mut sample = Vec::new();
            sample.extend_from_slice(b"field_name_");
            sample.extend_from_slice(&i.to_le_bytes());
            sample.extend_from_slice(b"_value_data_repeated_pattern");
            samples.push(sample);
        }
        let dict = zstd::dict::from_samples(&samples, 4096).expect("train dict");

        let data = b"field_name_42_value_data_repeated_pattern_extra";
        let ct = CompressionType::ZstdDict(3);
        let compressed = compress(data, ct, Some(&dict));
        let decompressed = decompress(&compressed, ct, data.len(), Some(&dict)).unwrap();
        assert_eq!(data.as_slice(), &decompressed);
    }

    #[test]
    fn lz4_compresses() {
        let data = vec![0u8; 10_000];
        let compressed = compress(&data, CompressionType::Lz4, None);
        assert!(compressed.len() < data.len());
    }

    #[test]
    fn zstd_compresses_better_than_lz4() {
        let mut data = Vec::with_capacity(100_000);
        for i in 0u32..25_000 {
            data.extend_from_slice(&i.to_le_bytes());
        }
        let lz4_size = compress(&data, CompressionType::Lz4, None).len();
        let zstd_size = compress(&data, CompressionType::Zstd(3), None).len();
        assert!(
            zstd_size < lz4_size,
            "zstd({zstd_size}) should be smaller than lz4({lz4_size})"
        );
    }
}
