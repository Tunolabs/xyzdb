// SPDX-License-Identifier: BUSL-1.1
// Z-Order 2D encoding/decoding. Ported from POC 2 (validated at 123M records).

/// Spread 21 bits of a value into every other bit position (42 bits total).
/// Input bit i goes to output bit i*2.
fn spread_bits_21(v: u32) -> u64 {
    let mut result: u64 = 0;
    for i in 0..21 {
        result |= ((v as u64 >> i) & 1) << (i * 2);
    }
    result
}

/// Compact every other bit from 42 bits back into 21 bits.
fn compact_bits_21(z: u64) -> u32 {
    let mut result: u32 = 0;
    for i in 0..21 {
        result |= (((z >> (i * 2)) & 1) as u32) << i;
    }
    result
}

/// Z-Order 2D encoding: interleave bits of y and z into 42 bits.
/// y occupies even bit positions (0, 2, 4, ..., 40).
/// z occupies odd bit positions (1, 3, 5, ..., 41).
/// y, z each max 21 bits (0..2_097_151).
pub fn z_order_2d_encode(y: u32, z: u32) -> u64 {
    spread_bits_21(y & 0x1F_FFFF) | (spread_bits_21(z & 0x1F_FFFF) << 1)
}

/// Decode Z-Order 2D back into (y, z).
pub fn z_order_2d_decode(val: u64) -> (u32, u32) {
    let y = compact_bits_21(val);
    let z = compact_bits_21(val >> 1);
    (y, z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        for &(y, z) in &[
            (0, 0),
            (1, 1),
            (1000, 500),
            (0x1F_FFFF, 0x1F_FFFF),
            (42, 99),
        ] {
            let encoded = z_order_2d_encode(y, z);
            let (dy, dz) = z_order_2d_decode(encoded);
            assert_eq!((dy, dz), (y, z), "roundtrip failed for ({y}, {z})");
        }
    }

    #[test]
    fn encoded_fits_42_bits() {
        let encoded = z_order_2d_encode(0x1F_FFFF, 0x1F_FFFF);
        assert!(encoded <= 0x3FF_FFFF_FFFF, "exceeds 42 bits");
    }

    #[test]
    fn zero_encodes_to_zero() {
        assert_eq!(z_order_2d_encode(0, 0), 0);
    }
}
