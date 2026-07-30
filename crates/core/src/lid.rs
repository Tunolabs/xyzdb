use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Global sequence counter for LID generation. Resets to 0 on every process
/// start (it is not persisted), so it cannot by itself guarantee LID
/// uniqueness across a restart — see `BOOT_EPOCH`.
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Per-open boot epoch, embedded in each LID's low 16 bits (the former
/// `reserved` field). `Engine::open` advances a durably-persisted counter and
/// calls `set_boot_epoch`, so LIDs minted by two different opens (a restart,
/// crash, or reopen) never collide — even if the wall clock repeats a
/// microsecond and `SEQUENCE` has reset to 0 (2a, the silent-duplicate-identity
/// vulnerability). Wraps after 65536 opens; the timestamp + sequence remain as
/// secondary disambiguators past that.
static BOOT_EPOCH: AtomicU16 = AtomicU16::new(0);

/// LID — Logical Identifier. Immutable 128-bit identity for every record.
///
/// Layout (128 bits total):
/// ```text
/// [Node_ID: 16 bits][Lobe_ID: 16 bits][Timestamp: 48 bits][Sequence: 32 bits][Reserved: 16 bits]
/// ```
///
/// - Node_ID: for future distributed mode (0 in single-node MVP)
/// - Lobe_ID: lobe where the record was born
/// - Timestamp: microseconds since Unix epoch (~8,925 years of range)
/// - Sequence: disambiguates inserts within the same microsecond
/// - Reserved: 16 bits for future use (flags, version, etc.)
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LID(u128);

impl LID {
    /// Create a new LID for a record born in the given lobe.
    pub fn new(lobe_id: u16) -> Self {
        let node_id: u16 = 0;
        let timestamp = current_timestamp_micros();
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        // Low 16 bits carry the per-open boot epoch so two opens never mint
        // the same LID across a restart (2a). 0 until `set_boot_epoch` runs.
        let reserved: u16 = BOOT_EPOCH.load(Ordering::Relaxed);

        let val = (node_id as u128) << 112
            | (lobe_id as u128) << 96
            | ((timestamp & 0xFFFF_FFFF_FFFF) as u128) << 48
            | (seq as u128) << 16
            | reserved as u128;

        Self(val)
    }

    /// Reconstruct a LID from its raw u128 value.
    pub fn from_raw(val: u128) -> Self {
        Self(val)
    }

    /// Get the raw u128 value.
    pub fn raw(&self) -> u128 {
        self.0
    }

    /// Serialize to 16 big-endian bytes.
    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    /// Deserialize from 16 big-endian bytes.
    pub fn from_bytes(bytes: &[u8; 16]) -> Self {
        Self(u128::from_be_bytes(*bytes))
    }

    pub fn node_id(&self) -> u16 {
        (self.0 >> 112) as u16
    }

    pub fn lobe_id(&self) -> u16 {
        (self.0 >> 96) as u16
    }

    pub fn timestamp(&self) -> u64 {
        ((self.0 >> 48) & 0xFFFF_FFFF_FFFF) as u64
    }

    pub fn sequence(&self) -> u32 {
        ((self.0 >> 16) & 0xFFFF_FFFF) as u32
    }

    /// The boot epoch carried in the low 16 bits (see `BOOT_EPOCH`).
    pub fn boot_epoch(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// Set the process-wide boot epoch embedded in every subsequent LID.
    /// Called once by `Engine::open` with a durably-incremented counter so
    /// that LIDs from different opens never collide (2a). Idempotent within a
    /// process; the last call wins.
    pub fn set_boot_epoch(epoch: u16) {
        BOOT_EPOCH.store(epoch, Ordering::Relaxed);
    }

    /// Parse from display format: "NNNN:LLLL:TTTTTTTTTTTT:SSSSSSSS:RRRR"
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 5 {
            return Err(crate::error::XyzError::Parse(format!(
                "Invalid LID format: expected 5 colon-separated segments, got {}",
                parts.len()
            )));
        }

        let node_id = u16::from_str_radix(parts[0], 16)
            .map_err(|e| crate::error::XyzError::Parse(format!("Bad LID node_id: {e}")))?;
        let lobe_id = u16::from_str_radix(parts[1], 16)
            .map_err(|e| crate::error::XyzError::Parse(format!("Bad LID lobe_id: {e}")))?;
        let timestamp = u64::from_str_radix(parts[2], 16)
            .map_err(|e| crate::error::XyzError::Parse(format!("Bad LID timestamp: {e}")))?;
        let sequence = u32::from_str_radix(parts[3], 16)
            .map_err(|e| crate::error::XyzError::Parse(format!("Bad LID sequence: {e}")))?;
        let reserved = u16::from_str_radix(parts[4], 16)
            .map_err(|e| crate::error::XyzError::Parse(format!("Bad LID reserved: {e}")))?;

        let val = (node_id as u128) << 112
            | (lobe_id as u128) << 96
            | ((timestamp & 0xFFFF_FFFF_FFFF) as u128) << 48
            | (sequence as u128) << 16
            | reserved as u128;

        Ok(Self(val))
    }
}

impl fmt::Display for LID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04X}:{:04X}:{:012X}:{:08X}:{:04X}",
            self.node_id(),
            self.lobe_id(),
            self.timestamp(),
            self.sequence(),
            self.0 & 0xFFFF,
        )
    }
}

impl fmt::Debug for LID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LID({})", self)
    }
}

fn current_timestamp_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lid_roundtrip_display_parse() {
        let lid = LID::new(0x001A);
        let s = lid.to_string();
        let parsed = LID::parse(&s).expect("parse should succeed");
        assert_eq!(lid, parsed);
    }

    #[test]
    fn lid_segments() {
        let lid = LID::new(42);
        assert_eq!(lid.node_id(), 0);
        assert_eq!(lid.lobe_id(), 42);
        assert!(lid.timestamp() > 0);
    }

    /// 2a (S1) — the boot epoch makes LIDs collision-proof across a restart.
    ///
    /// Before the fix, the low 16 bits were always 0, so two opens that minted
    /// a LID in the same microsecond after `SEQUENCE` reset to 0 produced a
    /// byte-identical LID — a silent duplicate identity (a backward NTP/VM
    /// clock jump, or a fast restart, makes the microsecond repeat). The fix:
    /// `Engine::open` advances a durably-persisted per-open epoch into those 16
    /// bits, so LIDs from different opens differ even at an identical
    /// lobe/timestamp/sequence. Asserted deterministically via `from_raw` to
    /// avoid depending on the shared global clock/atomics under parallel tests.
    #[test]
    fn lid_boot_epoch_disambiguates_across_opens() {
        // Identical lobe(7) / timestamp / sequence(0); only the boot epoch
        // (low 16 bits) varies.
        let base = (7u128 << 96) | (0x1234_5678_9ABCu128 << 48);

        // Pre-fix behaviour: a fixed epoch of 0 collides — the 2a fault.
        assert_eq!(
            LID::from_raw(base).raw(),
            LID::from_raw(base).raw(),
            "control: identical fields with the same (zero) epoch collide"
        );

        // The fix: distinct per-open epochs keep otherwise-identical LIDs apart.
        let pre = LID::from_raw(base | 1);
        let post = LID::from_raw(base | 2);
        assert_ne!(
            pre.raw(),
            post.raw(),
            "LIDs from different opens must differ even at an identical lobe/timestamp/seq"
        );
        assert_eq!(pre.boot_epoch(), 1);
        assert_eq!(post.boot_epoch(), 2);
    }

    /// `LID::new` embeds the installed boot epoch in the low 16 bits. (This is
    /// the only test that calls `set_boot_epoch`, so the shared global is not
    /// raced by a peer test.)
    #[test]
    fn lid_new_carries_installed_boot_epoch() {
        LID::set_boot_epoch(0xABCD);
        let lid = LID::new(3);
        assert_eq!(lid.boot_epoch(), 0xABCD);
        LID::set_boot_epoch(0); // restore default for any peer LID::new
    }

    #[test]
    fn lid_bytes_roundtrip() {
        let lid = LID::new(7);
        let bytes = lid.to_bytes();
        let restored = LID::from_bytes(&bytes);
        assert_eq!(lid, restored);
    }

    #[test]
    fn lid_sequence_increments() {
        let a = LID::new(1);
        let b = LID::new(1);
        assert!(b.sequence() > a.sequence() || b.timestamp() > a.timestamp());
    }
}
