//! The lobe's **sub-gravity axis** — a foundational axis, sibling to (NOT part
//! of) [`crate::gravity_spec::GravitySpec`] and [`crate::vector_spec::VectorSpec`].
//!
//! The three axes are orthogonal and must not be conflated:
//! - **Gravity** decides *placement*: which fields form the co-location hash,
//!   i.e. which gravity bucket a record lands in.
//! - **The searchable vector** decides which embedding NEAREST sweeps.
//! - **The satellite** decides how a *single* gravity bucket is *sub-divided*:
//!   the field whose value maps to the `sat` axis of the spatial key, so a
//!   bounded query scans one satellite instead of the whole parent bucket.
//!
//! This is equality-only sub-bucketing, not an index and not range gravity: the
//! `sat` axis co-locates records sharing a field value within their parent
//! bucket, making the exact scan over that sub-set cheap to materialise.
//!
//! Persisted in the dictionary slot `[SATELLITE][lobe_id]` as
//! `[XYZDB_MAGIC][SPEC_FORMAT][postcard(SatelliteSpec)]` — the same envelope
//! shape as `GravitySpec`/`VectorSpec`, so a lobe reads its axes from one place.

use serde::{Deserialize, Serialize};

/// Current persisted format byte, behind `XYZDB_MAGIC` in the `[0xFF,0xF5]
/// [lobe_id]` dictionary slot. `0x01` is the only format: a single field name.
const SPEC_FORMAT: u8 = 0x01;

/// The lobe's sub-gravity axis: the single field whose value maps to the `sat`
/// axis of the spatial key. One per lobe (§7.1 of the sub-gravity evaluation:
/// one axis per lobe, declared — a second candidate field cannot share the u16).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteSpec {
    /// Name of the field whose value sub-buckets the gravity bucket.
    pub field: String,
}

impl SatelliteSpec {
    /// Build a spec for the given field name.
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
        }
    }

    /// Encode for the dictionary slot as `[XYZDB_MAGIC][SPEC_FORMAT][postcard]`.
    ///
    /// # Errors
    /// Returns the postcard error if the spec fails to serialize.
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        let mut bytes = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(SPEC_FORMAT);
        bytes.extend_from_slice(&postcard::to_allocvec(self)?);
        Ok(bytes)
    }

    /// Decode a slot value written by [`SatelliteSpec::encode`]. Returns `None`
    /// for an unrecognised envelope (wrong magic or format byte).
    pub fn decode(bytes: &[u8]) -> Option<SatelliteSpec> {
        if bytes.len() < 3 || bytes[0..2] != xyzdb_core::record::XYZDB_MAGIC {
            return None;
        }
        match bytes[2] {
            SPEC_FORMAT => postcard::from_bytes(&bytes[3..]).ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let spec = SatelliteSpec::new("kind");
        let bytes = spec.encode().unwrap();
        assert_eq!(&bytes[0..2], &xyzdb_core::record::XYZDB_MAGIC);
        assert_eq!(bytes[2], SPEC_FORMAT, "current format is 0x01");
        assert_eq!(SatelliteSpec::decode(&bytes), Some(spec));
    }

    #[test]
    fn decode_rejects_foreign_envelope() {
        assert_eq!(SatelliteSpec::decode(b"\x00\x00\x01junk"), None);
        assert_eq!(SatelliteSpec::decode(&[]), None);
        // Right magic, unknown format byte.
        let mut wrong = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        wrong.push(0x7F);
        assert_eq!(SatelliteSpec::decode(&wrong), None);
    }
}
