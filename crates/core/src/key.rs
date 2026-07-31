use crate::zorder::z_order_2d_encode;

/// Size of a serialized SpatialKey in bytes (0.9.4 layout: the 22-byte
/// v0.6.0-pre layout widened by a reserved 2-byte satellite axis — see
/// [`SpatialKey`]).
pub const SPATIAL_KEY_SIZE: usize = 24;

/// 48-bit width used by gravity_hash. Stored as 6 BE bytes on disk;
/// callers receive a `u64` with only the low 48 bits populated.
pub const GRAVITY_HASH_BITS: u32 = 48;

/// Bit mask for `GRAVITY_HASH_BITS` (low 48 bits set).
pub const GRAVITY_HASH_MASK: u64 = (1u64 << GRAVITY_HASH_BITS) - 1;

/// Prefix length in bytes for a gravity-bounded range scan
/// (`lobe_id` + `gravity_hash`).
pub const GRAVITY_PREFIX_SIZE: usize = 2 + 6;

/// Spatial key for the primary Turba keyspace: 24 bytes big-endian
/// (0.9.4 layout; v0.6.0-pre used 22 bytes, v0.5.x and earlier 18).
///
/// Layout:
/// ```text
/// [lobe_id: u16 BE = 2 bytes]        ← bytes 0..2
/// [gravity_hash: u48 BE = 6 bytes]   ← bytes 2..8    primary grouping
/// [sat: u16 BE = 2 bytes]            ← bytes 8..10   satellite axis (RESERVED, always 0)
/// [z_order_2d: u48 BE = 6 bytes]     ← bytes 10..16  (type_id, timestamp_norm) interleaved
/// [seq: u64 BE = 8 bytes]            ← bytes 16..24  monotonic tiebreaker
/// ```
///
/// ## The `sat` (satellite) axis — RESERVED AT 0 ON PURPOSE (0.9.4)
///
/// `sat` is the physical slot for a future *sub-gravity* feature ("planets
/// with satellites": splitting one large gravity bucket into ordered
/// sub-buckets so a NEAREST scans a satellite, not the whole parent). The
/// FORMAT is reserved now — while there are zero users and no data to
/// migrate — but the LOGIC is deferred: [`SpatialKey::new`] always writes
/// `sat = 0`, so every record lives in satellite 0. Physical behaviour is
/// therefore IDENTICAL to the 22-byte layout — one flat bucket per
/// `(lobe_id, gravity_hash)`, and [`SpatialKey::prefix_for_gravity`] walks
/// the same range (it sweeps every `sat`, and every key is at `sat = 0`).
///
/// Nothing sets `sat != 0`. DEFERRED / out of scope until un-deferred: the
/// satellite router, the warming/split criterion, and observed re-packing.
/// The TRIGGER to un-defer is **measured pain on a real large gravity
/// bucket** (the interactive-latency ceiling, ~5–10k vectors/bucket). The
/// baseline to beat already exists in `benchmarks/agentic/` (superbucket
/// sweep: p50 250k = 645 ms, 500k = 1359 ms, recall 1.0). NOTE: *range* gravity (e.g. fintech date
/// ranges) is a DISTINCT sibling feature — this sub-gravity axis is
/// equality-only; do not conflate the two.
///
/// The on-disk format is incompatible with the 22-byte layout. Engines on
/// `MANIFEST_VERSION = 5` reject earlier data dirs (see
/// `turba_engine::manifest::read_manifest`). There is no in-place widening;
/// recreating the dataset from source is the migration path.
///
/// All records for the same gravity bucket share the first 8 bytes
/// (`lobe_id` + `gravity_hash`), so they sort contiguously in SST files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialKey {
    pub lobe_id: u16,
    /// Gravity hash. Stored as 48 bits on disk; the type is `u64` for
    /// ergonomic arithmetic.
    pub gravity_hash: u64,
    /// Satellite (sub-gravity) axis — RESERVED, always 0 (see struct doc).
    /// The physical slot for future sub-bucketing; no code sets it non-zero.
    pub sat: u16,
    pub type_id: u16,
    pub timestamp_norm: u32, // normalised timestamp, 21 bits effective
    pub seq: u64,            // monotonic uniqueness counter
}

#[inline]
fn u48_to_be_bytes(v: u64) -> [u8; 6] {
    let full = v.to_be_bytes();
    [full[2], full[3], full[4], full[5], full[6], full[7]]
}

#[inline]
fn u48_from_be_bytes(b: &[u8]) -> u64 {
    debug_assert!(b.len() >= 6);
    u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]])
}

impl SpatialKey {
    /// Build a spatial key from components.
    /// - `gravity_hash` is masked to 48 bits (`GRAVITY_HASH_MASK`).
    /// - `type_id` is used as-is for Z-Order encoding (low 21 bits).
    /// - `timestamp_norm` is masked to 21 bits.
    /// - `seq` is a monotonic u64 counter for uniqueness.
    pub fn new(
        lobe_id: u16,
        gravity_hash: u64,
        type_id: u16,
        timestamp_norm: u32,
        seq: u64,
    ) -> Self {
        Self {
            lobe_id,
            gravity_hash: gravity_hash & GRAVITY_HASH_MASK,
            // Satellite axis reserved at 0 (see struct doc). No constructor
            // parameter exposes it — sub-gravity materialization is deferred.
            sat: 0,
            type_id,
            timestamp_norm: timestamp_norm & 0x1F_FFFF,
            seq,
        }
    }

    /// Build a spatial key with an explicit satellite (`sat`) axis. Same as
    /// [`SpatialKey::new`] but places the record in sub-bucket `sat` of its
    /// gravity bucket instead of the default satellite 0. The write path calls
    /// this for a lobe with a declared `SatelliteSpec`; every other caller keeps
    /// using [`SpatialKey::new`] (sat 0), so the two paths cannot diverge.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_sat(
        lobe_id: u16,
        gravity_hash: u64,
        sat: u16,
        type_id: u16,
        timestamp_norm: u32,
        seq: u64,
    ) -> Self {
        Self {
            lobe_id,
            gravity_hash: gravity_hash & GRAVITY_HASH_MASK,
            sat,
            type_id,
            timestamp_norm: timestamp_norm & 0x1F_FFFF,
            seq,
        }
    }

    /// Serialize to the 24-byte big-endian representation for Turba
    /// storage. See struct doc for the layout.
    pub fn to_bytes(&self) -> [u8; SPATIAL_KEY_SIZE] {
        let z2d = z_order_2d_encode(self.type_id as u32 & 0x1F_FFFF, self.timestamp_norm);
        let mut buf = [0u8; SPATIAL_KEY_SIZE];
        buf[0..2].copy_from_slice(&self.lobe_id.to_be_bytes());
        buf[2..8].copy_from_slice(&u48_to_be_bytes(self.gravity_hash));
        buf[8..10].copy_from_slice(&self.sat.to_be_bytes());
        buf[10..16].copy_from_slice(&u48_to_be_bytes(z2d));
        buf[16..24].copy_from_slice(&self.seq.to_be_bytes());
        buf
    }

    /// Decode from the 24-byte big-endian representation.
    pub fn from_bytes(bytes: &[u8; SPATIAL_KEY_SIZE]) -> Self {
        let lobe_id = u16::from_be_bytes([bytes[0], bytes[1]]);
        let gravity_hash = u48_from_be_bytes(&bytes[2..8]);
        let sat = u16::from_be_bytes([bytes[8], bytes[9]]);
        let z2d = u48_from_be_bytes(&bytes[10..16]);
        let seq = u64::from_be_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]);
        let (type_id_raw, timestamp_norm) = crate::zorder::z_order_2d_decode(z2d);
        Self {
            lobe_id,
            gravity_hash,
            sat,
            type_id: type_id_raw as u16,
            timestamp_norm,
            seq,
        }
    }

    /// Extract `gravity_hash` from raw 22-byte key without full decode.
    pub fn gravity_hash_from_bytes(bytes: &[u8; SPATIAL_KEY_SIZE]) -> u64 {
        u48_from_be_bytes(&bytes[2..8])
    }

    /// Compute (start_key, end_key) for a range scan over all records
    /// in a given gravity bucket. Both bounds are full 22-byte keys so
    /// the underlying `range(min, max)` walks every record sharing the
    /// `(lobe_id, gravity_hash)` prefix regardless of `z_order_2d` or
    /// `seq`. `key_min` zeroes the tail; `key_max` saturates it.
    pub fn prefix_for_gravity(
        lobe_id: u16,
        gravity_hash: u64,
    ) -> ([u8; SPATIAL_KEY_SIZE], [u8; SPATIAL_KEY_SIZE]) {
        let gh = gravity_hash & GRAVITY_HASH_MASK;
        let mut key_min = [0u8; SPATIAL_KEY_SIZE];
        key_min[0..2].copy_from_slice(&lobe_id.to_be_bytes());
        key_min[2..8].copy_from_slice(&u48_to_be_bytes(gh));
        // bytes 8..24 already zero (sat + z_order_2d + seq).

        let mut key_max = [0u8; SPATIAL_KEY_SIZE];
        key_max[0..2].copy_from_slice(&lobe_id.to_be_bytes());
        key_max[2..8].copy_from_slice(&u48_to_be_bytes(gh));
        // Saturate the whole tail — sat (8..10), z_order_2d (10..16), seq
        // (16..24) — so the range sweeps EVERY satellite (all 0 today) and
        // every record sharing (lobe_id, gravity_hash). This is why the
        // reserved sat axis leaves the flat-bucket scan range unchanged.
        key_max[8..24].copy_from_slice(&[0xFF; 16]);
        (key_min, key_max)
    }

    /// Compute (start_key, end_key) for a range scan over ONE satellite
    /// (sub-bucket) of a gravity bucket: every record sharing
    /// `(lobe_id, gravity_hash, sat)`. Fixes bytes 0..10 (lobe + gravity + sat)
    /// and saturates only the tail (`z_order_2d` + `seq`, bytes 10..24), so the
    /// range walks the satellite in `z_order_2d → seq` order — the SAME order,
    /// on the SAME rows, that the parent-bucket scan ([`Self::prefix_for_gravity`])
    /// would emit for this satellite's rows (they are contiguous within the
    /// parent). This is why the bounded scan is a pure optimisation of the parent
    /// scan, not a different result.
    ///
    /// `hash16` collides (a `u16` axis), so this range can contain intruder rows
    /// from a different field value that hashed to the same `sat`. The caller
    /// MUST still apply the field predicate as a residual to drop them — the
    /// range narrows the candidates, the residual guarantees correctness.
    pub fn prefix_for_satellite(
        lobe_id: u16,
        gravity_hash: u64,
        sat: u16,
    ) -> ([u8; SPATIAL_KEY_SIZE], [u8; SPATIAL_KEY_SIZE]) {
        let gh = gravity_hash & GRAVITY_HASH_MASK;
        let sat_be = sat.to_be_bytes();

        let mut key_min = [0u8; SPATIAL_KEY_SIZE];
        key_min[0..2].copy_from_slice(&lobe_id.to_be_bytes());
        key_min[2..8].copy_from_slice(&u48_to_be_bytes(gh));
        key_min[8..10].copy_from_slice(&sat_be);
        // bytes 10..24 already zero (z_order_2d + seq).

        let mut key_max = [0u8; SPATIAL_KEY_SIZE];
        key_max[0..2].copy_from_slice(&lobe_id.to_be_bytes());
        key_max[2..8].copy_from_slice(&u48_to_be_bytes(gh));
        key_max[8..10].copy_from_slice(&sat_be);
        // Saturate only z_order_2d (10..16) + seq (16..24); sat is FIXED, so the
        // range stays inside this one satellite.
        key_max[10..24].copy_from_slice(&[0xFF; 14]);
        (key_min, key_max)
    }
}

/// Hash a string to 48 bits for use as `gravity_hash`.
/// FNV-1a then mask to 48 bits. Deterministic; no salt.
pub fn hash_to_48bits(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h & GRAVITY_HASH_MASK
}

/// Hash a string to 16 bits for use as the satellite (`sat`) axis.
/// FNV-1a folded to 16 bits (xor-fold, so all input bits reach the result).
/// Deterministic; no salt. The 16-bit width collides by design — the read path
/// applies the field predicate as a residual to guarantee exactness.
pub fn hash_to_16bits(s: &str) -> u16 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // Xor-fold the 64-bit hash down to 16 bits so high-entropy bits participate.
    ((h ^ (h >> 16) ^ (h >> 32) ^ (h >> 48)) & 0xFFFF) as u16
}

/// Normalise a microsecond timestamp to 21 bits.
/// Maps recent timestamps into a 21-bit range for Z-Order encoding.
/// Uses modular reduction — only needs local ordering, not global uniqueness.
pub fn normalize_timestamp(micros: u64) -> u32 {
    (micros & 0x1F_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spatial_key_roundtrip() {
        let key = SpatialKey::new(1, 0x1A3F7, 0, 12345, 42);
        let bytes = key.to_bytes();
        assert_eq!(bytes.len(), SPATIAL_KEY_SIZE);
        let decoded = SpatialKey::from_bytes(&bytes);
        assert_eq!(key.lobe_id, decoded.lobe_id);
        assert_eq!(key.gravity_hash, decoded.gravity_hash);
        assert_eq!(key.timestamp_norm, decoded.timestamp_norm);
        assert_eq!(key.seq, decoded.seq);
        // The satellite axis is reserved at 0 and round-trips as 0.
        assert_eq!(decoded.sat, 0);
    }

    #[test]
    fn spatial_key_size_is_24() {
        // 0.9.4: 22 → 24, the reserved 2-byte satellite axis. The gravity
        // prefix (lobe_id + gravity_hash) is unchanged at 8 bytes.
        assert_eq!(SPATIAL_KEY_SIZE, 24);
        assert_eq!(GRAVITY_PREFIX_SIZE, 8);
    }

    #[test]
    fn satellite_axis_is_reserved_at_zero() {
        // Nothing sets sat != 0 today: the constructor writes 0 and the byte
        // slot (8..10) is zero, so behaviour is identical to the 22B layout.
        let bytes = SpatialKey::new(3, 0xABCD, 1, 500, 7).to_bytes();
        assert_eq!(
            &bytes[8..10],
            &[0u8, 0u8],
            "sat slot must be zero (reserved)"
        );
    }

    #[test]
    fn gravity_hash_full_48_bits_roundtrip() {
        // Exercise the top of the 48-bit range; v0.5.x with a 21-bit
        // hash would truncate this to garbage.
        let big: u64 = 0xFFFF_FFFF_FFFF; // 48 ones
        let key = SpatialKey::new(7, big, 3, 1000, 99);
        let bytes = key.to_bytes();
        let decoded = SpatialKey::from_bytes(&bytes);
        assert_eq!(decoded.gravity_hash, big);
        let extracted = SpatialKey::gravity_hash_from_bytes(&bytes);
        assert_eq!(extracted, big);
    }

    #[test]
    fn same_entity_keys_contiguous() {
        let k1 = SpatialKey::new(1, 42, 0, 0, 0).to_bytes();
        let k2 = SpatialKey::new(1, 42, 0, 1000, 1).to_bytes();
        let other = SpatialKey::new(1, 43, 0, 0, 0).to_bytes();
        // First 8 bytes (prefix) determine bucket ordering.
        assert!(k1[..GRAVITY_PREFIX_SIZE] < other[..GRAVITY_PREFIX_SIZE]);
        assert!(k2[..GRAVITY_PREFIX_SIZE] < other[..GRAVITY_PREFIX_SIZE]);
    }

    #[test]
    fn prefix_covers_bucket() {
        let (min, max) = SpatialKey::prefix_for_gravity(1, 42);
        let k = SpatialKey::new(1, 42, 0, 999, 5).to_bytes();
        // k must sit inside the [min, max] range for the bucket.
        assert!(k >= min, "k must be >= bucket min");
        assert!(k <= max, "k must be <= bucket max");
        // The lobe + gravity prefix is identical to the bucket min.
        assert_eq!(&k[..GRAVITY_PREFIX_SIZE], &min[..GRAVITY_PREFIX_SIZE]);

        let outside = SpatialKey::new(1, 43, 0, 0, 0).to_bytes();
        assert!(outside > max, "neighbouring bucket must be above max");
    }

    #[test]
    // 48-bit gravity-hash fixtures; the byte grouping is cosmetic test data.
    #[allow(clippy::unusual_byte_groupings)]
    fn gravity_hash_extraction() {
        let key = SpatialKey::new(5, 0xABCDE_F012_3456, 0, 0, 99);
        let bytes = key.to_bytes();
        let extracted = SpatialKey::gravity_hash_from_bytes(&bytes);
        assert_eq!(extracted, 0xABCDE_F012_3456 & GRAVITY_HASH_MASK);
    }

    #[test]
    fn seq_preserves_uniqueness() {
        let k1 = SpatialKey::new(1, 42, 0, 100, 0).to_bytes();
        let k2 = SpatialKey::new(1, 42, 0, 100, 1).to_bytes();
        // Same entity/type/ts but different seq → different keys.
        assert_ne!(k1, k2);
        // First 14 bytes (lobe + gravity + z2d) are identical.
        assert_eq!(k1[..14], k2[..14]);
    }

    #[test]
    fn hash_deterministic() {
        let a = hash_to_48bits("ACME-001");
        let b = hash_to_48bits("ACME-001");
        assert_eq!(a, b);
        assert!(a <= GRAVITY_HASH_MASK);
    }

    #[test]
    fn hash16_deterministic() {
        assert_eq!(hash_to_16bits("click"), hash_to_16bits("click"));
        // Different values (usually) differ; this pair is just a smoke check.
        assert_ne!(hash_to_16bits("click"), hash_to_16bits("view"));
    }

    #[test]
    fn new_with_sat_roundtrips_nonzero_sat() {
        let key = SpatialKey::new_with_sat(1, 0x1A3F7, 0xBEEF, 3, 12345, 42);
        let bytes = key.to_bytes();
        // sat occupies bytes 8..10, big-endian.
        assert_eq!(&bytes[8..10], &0xBEEF_u16.to_be_bytes());
        let decoded = SpatialKey::from_bytes(&bytes);
        assert_eq!(decoded.sat, 0xBEEF);
        assert_eq!(decoded.gravity_hash, 0x1A3F7);
        assert_eq!(decoded.seq, 42);
    }

    #[test]
    fn satellite_prefix_covers_only_its_satellite() {
        let (min, max) = SpatialKey::prefix_for_satellite(1, 42, 7);
        // A key in satellite 7 of bucket (1,42) is inside [min, max].
        let inside = SpatialKey::new_with_sat(1, 42, 7, 0, 999, 5).to_bytes();
        assert!(inside >= min && inside <= max);
        // The same lobe+gravity but a NEIGHBOURING satellite is outside.
        let other_sat_hi = SpatialKey::new_with_sat(1, 42, 8, 0, 0, 0).to_bytes();
        assert!(
            other_sat_hi > max,
            "satellite 8 must sort above satellite 7's max"
        );
        let other_sat_lo = SpatialKey::new_with_sat(1, 42, 6, 0, 0xFFFFF, u64::MAX).to_bytes();
        assert!(
            other_sat_lo < min,
            "satellite 6 must sort below satellite 7's min"
        );
        // sat 0 (the default/dumpster) is also outside a sat=7 bounded scan.
        let default_sat = SpatialKey::new(1, 42, 0, 0, 0).to_bytes();
        assert!(
            default_sat < min,
            "satellite 0 must sort below satellite 7's min"
        );
    }

    #[test]
    fn satellite_prefix_is_a_subrange_of_the_parent() {
        // Every satellite range must sit within the parent gravity range, so a
        // bounded scan reads a subset of what the parent scan reads.
        let (pmin, pmax) = SpatialKey::prefix_for_gravity(1, 42);
        for sat in [0u16, 1, 7, 0x00FF, 0xBEEF, 0xFFFF] {
            let (smin, smax) = SpatialKey::prefix_for_satellite(1, 42, sat);
            assert!(
                smin >= pmin && smax <= pmax,
                "satellite {sat} escapes the parent range"
            );
        }
    }
}
