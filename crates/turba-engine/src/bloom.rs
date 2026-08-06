//! Bloom filter: probabilistic set membership test.
//!
//! Uses XXH3 double hashing: h(i) = h1 + i * h2
//! where h1 = lower 64 bits of XXH3-128, h2 = upper 64 bits.
//!
//! On-disk format:
//! ```text
//! [bit_array: N bytes]
//! [k: u8]               number of hash probes
//! [num_bits: u32 LE]    total bits in filter
//! ```

// SPDX-License-Identifier: BUSL-1.1
use byteorder_lite::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::Cursor;

/// Compute the XXH3-128 hash of a key, used for bloom filter probes.
pub fn key_hash(key: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_128(key) as u64
}

/// Build a bloom filter incrementally.
pub struct BloomBuilder {
    bits_per_key: f64,
    hashes: Vec<u64>,
}

impl BloomBuilder {
    pub fn new(bits_per_key: f64) -> Self {
        Self {
            bits_per_key,
            hashes: Vec::new(),
        }
    }

    pub fn insert(&mut self, hash: u64) {
        self.hashes.push(hash);
    }

    pub fn finish(self) -> Vec<u8> {
        if self.hashes.is_empty() {
            // Empty filter: 5-byte footer with k=0, num_bits=0
            let mut out = Vec::with_capacity(5);
            out.push(0); // k
            out.write_u32::<LittleEndian>(0).unwrap();
            return out;
        }

        // `num_bits` stays within u32 by construction: a single SSTable's
        // memtable is capped (≤ 32 MB default → ≤ ~500 K keys at realistic
        // record sizes). 500 K × 14 bits/key (HDD profile, max) ≈ 7 M
        // bits, three orders of magnitude below u32::MAX. If future scale
        // pushes a single SSTable past ~300 M items this assumption breaks
        // and the cast silently truncates — audit v0.2.1 tracked this
        // explicitly to prevent a future zone_maps-style bug.
        let num_bits = (self.hashes.len() as f64 * self.bits_per_key).ceil() as u32;
        // Round up to byte boundary, minimum 64 bits
        let num_bits = num_bits.max(64);
        let num_bytes = num_bits.div_ceil(8) as usize;
        let num_bits = (num_bytes * 8) as u32;

        // Optimal k = bits_per_key * ln(2) ≈ bits_per_key * 0.693
        let k = ((self.bits_per_key * 0.693) as u8).clamp(1, 30);

        let mut bits = vec![0u8; num_bytes];

        for hash in &self.hashes {
            let h1 = *hash;
            let h2 = (*hash).rotate_left(32); // rotate for second hash
            for i in 0..k as u64 {
                let bit_pos = (h1.wrapping_add(i.wrapping_mul(h2))) % (num_bits as u64);
                bits[(bit_pos / 8) as usize] |= 1 << (bit_pos % 8);
            }
        }

        // Append footer: k + num_bits
        let mut out = bits;
        out.push(k);
        out.write_u32::<LittleEndian>(num_bits).unwrap();
        out
    }
}

/// Read-only bloom filter.
pub struct BloomFilter {
    bits: Vec<u8>,
    k: u8,
    num_bits: u32,
}

impl BloomFilter {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 5 {
            return None;
        }
        let footer_start = data.len() - 5;
        let k = data[footer_start];
        let mut cursor = Cursor::new(&data[footer_start + 1..]);
        let num_bits = cursor.read_u32::<LittleEndian>().ok()?;

        if num_bits == 0 && k == 0 {
            return Some(Self {
                bits: Vec::new(),
                k: 0,
                num_bits: 0,
            });
        }

        let expected_bytes = num_bits.div_ceil(8) as usize;
        if footer_start != expected_bytes {
            return None;
        }

        Some(Self {
            bits: data[..footer_start].to_vec(),
            k,
            num_bits,
        })
    }

    /// Bytes of resident bloom bit array (excluding the struct header).
    pub fn size_bytes(&self) -> usize {
        self.bits.len()
    }

    pub fn maybe_contains(&self, hash: u64) -> bool {
        if self.num_bits == 0 {
            return true; // empty filter matches everything
        }

        let h1 = hash;
        let h2 = hash.rotate_left(32);
        for i in 0..self.k as u64 {
            let bit_pos = (h1.wrapping_add(i.wrapping_mul(h2))) % (self.num_bits as u64);
            if self.bits[(bit_pos / 8) as usize] & (1 << (bit_pos % 8)) == 0 {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_contains_inserted_keys() {
        let mut builder = BloomBuilder::new(10.0);
        let keys: Vec<_> = (0..1000u32).map(|i| format!("key_{i:06}")).collect();
        for k in &keys {
            builder.insert(key_hash(k.as_bytes()));
        }
        let data = builder.finish();
        let filter = BloomFilter::from_bytes(&data).unwrap();

        for k in &keys {
            assert!(
                filter.maybe_contains(key_hash(k.as_bytes())),
                "false negative for {k}"
            );
        }
    }

    #[test]
    fn bloom_false_positive_rate() {
        let mut builder = BloomBuilder::new(10.0);
        for i in 0..10_000u32 {
            builder.insert(key_hash(format!("present_{i}").as_bytes()));
        }
        let data = builder.finish();
        let filter = BloomFilter::from_bytes(&data).unwrap();

        let mut false_positives = 0;
        let absent_count = 100_000;
        for i in 0..absent_count {
            if filter.maybe_contains(key_hash(format!("absent_{i}").as_bytes())) {
                false_positives += 1;
            }
        }
        let fpr = false_positives as f64 / absent_count as f64;
        // At 10 bits/key, theoretical FPR ≈ 0.8%
        assert!(
            fpr < 0.02,
            "false positive rate {fpr:.4} exceeds 2% threshold"
        );
    }

    #[test]
    fn bloom_empty_filter() {
        let builder = BloomBuilder::new(10.0);
        let data = builder.finish();
        let filter = BloomFilter::from_bytes(&data).unwrap();
        // Empty filter should match everything (safe default)
        assert!(filter.maybe_contains(12345));
    }
}
