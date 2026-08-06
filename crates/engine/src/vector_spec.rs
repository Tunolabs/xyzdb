//! The lobe's designated **searchable vector field** — a foundational axis,
//! sibling to (NOT part of) [`crate::gravity_spec::GravitySpec`].
//!
//! The two are different axes and must not be conflated:
//! - **Gravity** decides *placement*: which fields form the co-location hash,
//!   i.e. which bucket a record lands in.
//! - **The searchable vector** decides which embedding is hoisted to the V3
//!   record prefix and swept by NEAREST. A record co-locates by (say) topic and
//!   searches by its embedding — orthogonal concerns.
//!
//! This is **not** clustering / IVF / any ANN index (cf. the `Centroid` roadmap
//! note in `gravity_spec.rs`, which is a *placement* idea and explicitly out of
//! scope). There is no index here: the prefix only makes the **exact** brute-
//! force scan over the gravity bucket cheap to materialise.
//!
//! Persisted in the dictionary slot `[VECTOR_FIELD][lobe_id]` as
//! `[XYZDB_MAGIC][SPEC_FORMAT][postcard(VectorSpec)]` — the same envelope shape
//! as `GravitySpec`, so `PUT` reads the lobe's hoist field from one place.

// SPDX-License-Identifier: BUSL-1.1
use serde::{Deserialize, Serialize};

/// Current persisted format byte, behind `XYZDB_MAGIC` in the
/// `[0xFF,0xF7][lobe_id]` dictionary slot. `0x01` was field-only; `0x02` adds
/// the learned dimension. A `0x01` slot still decodes (dim unknown → `None`).
const SPEC_FORMAT: u8 = 0x02;

/// The lobe's searchable vector field: the single embedding field hoisted to
/// the V3 record prefix and swept (exactly) by NEAREST. One per lobe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorSpec {
    /// Name of the `Value::Vector` field promoted to the on-disk prefix.
    pub field: String,
    /// Dimension the field defends: learned from the FIRST embedding written to
    /// it, then enforced — a later PUT of a different length errors instead of
    /// being silently unsearchable (cosine across mismatched dims is meaningless).
    /// `None` on a freshly declared spec, and on a legacy `0x01` spec loaded from
    /// disk, until the first PUT sets it. Flexibility is intact: each field is
    /// whatever dim it wants; only mixing dims WITHIN one field is closed.
    pub dim: Option<u32>,
}

impl VectorSpec {
    /// Build a spec for the given field name; dimension unknown until the first
    /// vector is written.
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            dim: None,
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

    /// Decode a slot value written by [`VectorSpec::encode`]. Returns `None` for
    /// an unrecognised envelope (wrong magic or format byte).
    pub fn decode(bytes: &[u8]) -> Option<VectorSpec> {
        if bytes.len() < 3 || bytes[0..2] != xyzdb_core::record::XYZDB_MAGIC {
            return None;
        }
        match bytes[2] {
            // Legacy field-only spec (retro-compat gate): a lobe written by an
            // older build opens and works unchanged; its dimension is unknown and
            // is learned on the next PUT that hoists the field.
            0x01 => {
                #[derive(Deserialize)]
                struct V1 {
                    field: String,
                }
                postcard::from_bytes::<V1>(&bytes[3..])
                    .ok()
                    .map(|v| VectorSpec {
                        field: v.field,
                        dim: None,
                    })
            }
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
        // Freshly declared: dim unknown.
        let spec = VectorSpec::new("embedding");
        let bytes = spec.encode().unwrap();
        assert_eq!(&bytes[0..2], &xyzdb_core::record::XYZDB_MAGIC);
        assert_eq!(bytes[2], SPEC_FORMAT, "current format is 0x02");
        assert_eq!(VectorSpec::decode(&bytes), Some(spec));
    }

    #[test]
    fn encode_decode_roundtrip_with_learned_dim() {
        let spec = VectorSpec {
            field: "emb".into(),
            dim: Some(768),
        };
        let bytes = spec.encode().unwrap();
        assert_eq!(bytes[2], SPEC_FORMAT);
        assert_eq!(VectorSpec::decode(&bytes), Some(spec));
    }

    /// The retro-compat gate: a legacy `0x01` slot (field-only, written by a
    /// build before dim tracking) must still decode — to the same field, with the
    /// dimension unknown (`None`) so it is learned on the next PUT. An old lobe
    /// opens and works unchanged; it never fails to load.
    #[test]
    fn decode_legacy_0x01_slot_yields_dim_none() {
        // Reconstruct exactly what the old encoder wrote: MAGIC ++ 0x01 ++
        // postcard(field). (A 1-field struct postcard-encodes as just the field.)
        let mut legacy = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        legacy.push(0x01);
        legacy.extend_from_slice(&postcard::to_allocvec(&"embedding".to_string()).unwrap());
        assert_eq!(
            VectorSpec::decode(&legacy),
            Some(VectorSpec {
                field: "embedding".into(),
                dim: None
            }),
            "legacy 0x01 spec must open with dim unknown"
        );
    }

    #[test]
    fn decode_rejects_foreign_envelope() {
        assert_eq!(VectorSpec::decode(b"\x00\x00\x01junk"), None);
        assert_eq!(VectorSpec::decode(&[]), None);
    }
}
