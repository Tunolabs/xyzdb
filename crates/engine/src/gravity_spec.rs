//! `GravitySpec` — the single source of truth for how a lobe derives its
//! gravity hash.
//!
//! Pre-0.8, the gravity hash was computed in two places that could disagree:
//! placement (WRITE) folded *every* `*`-marked field, while the SCAN fast path
//! (QUERY) used only the first-registered field. With two `*` fields the two
//! hashes diverged and the fast path looked in the wrong bucket — the
//! "two-`*` footgun" (Scenario D). This type makes both sides resolve the gravity
//! hash through one value, so they cannot diverge by construction.
//!
//! The roadmap's three axes (normalized / derived / composite / IVF) become
//! variants of this enum rather than special cases. **Fase 0** (this module,
//! no on-disk format change) wires `Raw`, `Normalized` and `Composite`; the
//! `Raw` path is byte-identical to the pre-0.8 hash because it feeds the same
//! `(name, value)` pair into the same [`crate::ops::put::compute_gravity_hash`]
//! primitive. `Derived` (Fase 1) and `Centroid`/vectors (Fase 4) land later.
//!
//! Persistence, the `GRAVITY BY` grammar, the SCAN bound resolution
//! (`bounds_for_where`) and the call-site rewrites in `ops/put.rs` and
//! `ops/scan.rs` are the **wiring increment** that follows this module.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xyzdb_core::value::Value;

/// A record's fields, as the engine holds them during a write/query.
type Fields = BTreeMap<String, Value>;

/// Current persisted `GravitySpec` format byte (0.8 D1), behind `XYZDB_MAGIC` in
/// the `[0xFF,0xFA][lobe_id]` dictionary slot. `0x03` marks the **value-only**
/// hash convention: a lobe at this byte has had its `*`-placed records rehashed.
const SPEC_FORMAT: u8 = 0x03;

/// Fase-0 format byte (`0x02`): a postcard `GravitySpec`, but its `*`-placed
/// records were hashed under the pre-D1 **name+value** convention. Decodes to the
/// same spec; the slot's presence at this byte marks the lobe as un-migrated.
const LEGACY_SPEC_FORMAT: u8 = 0x02;

/// Pre-Fase-0 format byte (`0x01`): the slot held `[MAGIC][0x01][postcard(String)]`,
/// a bare gravity field name. Decodes as `Raw(name)`. Also name+value era.
const LEGACY_FIELD_FORMAT: u8 = 0x01;

/// A value transform applied before hashing, for normalized gravity.
///
/// Opt-in (declared via `GRAVITY BY lower(field)`); it changes the bucket of
/// the affected field, so it is never inferred-and-applied silently. Only
/// identity-safe folds are exposed here; richer transforms (`Nfc`,
/// `NumericCanon`) are deferred until they carry their dependency and a
/// deliberate decision about identity semantics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transform {
    /// Case-fold: `"Acme"` and `"acme"` gravitate together.
    Lower,
    /// Strip leading/trailing whitespace.
    Trim,
    // TODO(0.8.x): Nfc — Unicode NFC; needs the `unicode-normalization` crate.
    // TODO(0.8.x): NumericCanon — unify `Int(5)` / `Float(5.0)` / `"5"`.
}

impl Transform {
    /// Apply the transform to a field's canonical string form.
    fn apply(&self, s: &str) -> String {
        match self {
            Transform::Lower => s.to_lowercase(),
            Transform::Trim => s.trim().to_string(),
        }
    }
}

/// How a lobe computes its gravity hash. WRITE (placement) and QUERY (the SCAN
/// equality fast path) both resolve through the same `GravitySpec`, so the
/// write-side and query-side hashes can never disagree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GravitySpec {
    /// A single field, hashed as-is. The pre-0.8 default; a bare `*field` and a
    /// legacy persisted field-name both mean `Raw(field)`. Byte-identical to
    /// the pre-0.8 gravity hash (same pair, same primitive).
    Raw(String),
    /// A single field with a value transform applied before hashing. Opt-in;
    /// changes the bucket of that field (e.g. case-insensitive co-location).
    Normalized(String, Transform),
    /// An ordered tuple of fields — the declared form of multi-field gravity
    /// (e.g. multi-tenant `(tenant, doc)`). Resolves the two-`*` footgun: both
    /// WRITE and QUERY fold the full tuple. Order is significant (it fixes the
    /// hash input and the future prefix-scan order, roadmap Fase 5).
    Composite(Vec<String>),
    // Fase 1: Derived(DerivedKey)  — bucket(f,unit) | prefix(f,n) | geocell(...)
    // Fase 4: Centroid(String)     — IVF cell over a vector field
}

impl GravitySpec {
    /// The gravity field name a bare `*field` or a legacy persisted string
    /// maps to. Used by the persistence layer to read pre-0.8 records with zero
    /// migration.
    #[cfg(test)]
    pub fn from_legacy_field(name: impl Into<String>) -> Self {
        GravitySpec::Raw(name.into())
    }

    /// Encode for the dictionary keyspace slot `[0xFF,0xFA][lobe_id]` as
    /// `[XYZDB_MAGIC][SPEC_FORMAT][postcard(GravitySpec)]`.
    ///
    /// The format byte ([`SPEC_FORMAT`] = `0x03`) marks the value-only (D1) hash
    /// convention. [`decode`] also reads the two pre-D1 bytes — `0x02` (Fase-0
    /// postcard spec) and `0x01` (bare field name → `Raw`) — so older slots load
    /// as the same spec; [`slot_is_pre_d1`] then flags the lobe for `migrate`.
    ///
    /// # Errors
    /// Returns the postcard error if the spec fails to serialize.
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        let payload = postcard::to_allocvec(self)?;
        let mut bytes = Vec::with_capacity(3 + payload.len());
        bytes.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(SPEC_FORMAT);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Decode a persisted gravity slot. Accepts the value-only D1 format
    /// (`[MAGIC][0x03][postcard(GravitySpec)]`), the Fase-0 format
    /// (`[MAGIC][0x02][postcard(GravitySpec)]`), and the pre-Fase-0 field-name
    /// format (`[MAGIC][0x01][postcard(String)]` → `Raw`) — so a slot loads as the
    /// same spec regardless of convention. Returns `None` if the magic or format
    /// byte is unrecognised (the caller treats it as "no spec").
    pub fn decode(bytes: &[u8]) -> Option<GravitySpec> {
        if bytes.len() < 3 || bytes[0..2] != xyzdb_core::record::XYZDB_MAGIC {
            return None;
        }
        match bytes[2] {
            SPEC_FORMAT | LEGACY_SPEC_FORMAT => postcard::from_bytes(&bytes[3..]).ok(),
            LEGACY_FIELD_FORMAT => postcard::from_bytes::<String>(&bytes[3..])
                .ok()
                .map(GravitySpec::Raw),
            _ => None,
        }
    }

    /// True if a persisted gravity slot uses a pre-D1 (name+value) format byte
    /// (`0x01` or `0x02`). Such a lobe's `*`-placed records live in name+value
    /// buckets and must be rehashed (`migrate`) before the value-only fast path
    /// finds them; the engine refuses data ops on the database until then.
    pub(crate) fn slot_is_pre_d1(bytes: &[u8]) -> bool {
        bytes.len() >= 3
            && bytes[0..2] == xyzdb_core::record::XYZDB_MAGIC
            && matches!(bytes[2], LEGACY_FIELD_FORMAT | LEGACY_SPEC_FORMAT)
    }

    /// The field names this spec reads. WRITE needs all of them present;
    /// QUERY needs an `Eq` on each to pin the gravity bucket.
    pub fn fields(&self) -> Vec<&str> {
        match self {
            GravitySpec::Raw(f) | GravitySpec::Normalized(f, _) => vec![f.as_str()],
            GravitySpec::Composite(fs) => fs.iter().map(String::as_str).collect(),
        }
    }

    /// Compute the gravity hash for `fields` under this spec, or `None` when a
    /// required field is absent (the caller then falls back to anchor/LID
    /// gravity, exactly as the pre-0.8 path did).
    ///
    /// This is the single source of truth: placement and the SCAN fast path
    /// both call it, so they cannot diverge.
    pub fn compute_hash(&self, fields: &Fields) -> Option<u64> {
        let pairs = self.hash_pairs(fields)?;
        Some(crate::ops::put::compute_gravity_hash(&pairs))
    }

    /// Query-side counterpart of [`Self::compute_hash`]: given a `WHERE`'s
    /// flattened `Eq` predicates, the gravity hash the SCAN fast path should
    /// bound to, or `None` if the predicate does not pin the bucket (caller
    /// falls back to a full scan). The caller turns the hash into a key range
    /// via `SpatialKey::prefix_for_gravity`.
    ///
    /// Pins only when *every* gravity field appears with exactly one `Eq`
    /// (a non-`Eq` or a duplicate on a gravity field disqualifies, matching the
    /// pre-0.8 `detect_gravity_eq`). For `Raw` this is byte-identical to the
    /// old path; for `Normalized` the `Eq` value is folded the same way the
    /// write side folded it, so `WHERE email = "Acme"` finds rows stored under
    /// `"acme"`; for `Composite` it requires the full tuple.
    pub fn pinned_gravity_hash(
        &self,
        filters: &[(String, xyzdb_core::record::FilterOp, Value)],
    ) -> Option<u64> {
        use xyzdb_core::record::FilterOp;
        let mut vals: Fields = BTreeMap::new();
        for field in self.fields() {
            let mut found: Option<Value> = None;
            for (f, op, v) in filters {
                if f == field {
                    if !matches!(op, FilterOp::Eq) {
                        return None; // a range/≠ on a gravity field → no fast path
                    }
                    if found.is_some() {
                        return None; // two Eq on the same gravity field → bail conservatively
                    }
                    found = Some(v.clone());
                }
            }
            vals.insert(field.to_string(), found?); // gravity field absent → full scan
        }
        self.compute_hash(&vals)
    }

    /// The `(name, value)` pairs this spec feeds into the gravity-hash
    /// primitive. `Raw`/`Composite` pass values through unchanged — so `Raw` is
    /// byte-identical to the pre-0.8 single-field path — while `Normalized`
    /// rewrites the value through its transform before hashing. Returns `None`
    /// if any required field is missing.
    fn hash_pairs(&self, fields: &Fields) -> Option<Vec<(String, Value)>> {
        match self {
            GravitySpec::Raw(field) => {
                let v = fields.get(field)?.clone();
                Some(vec![(field.clone(), v)])
            }
            GravitySpec::Normalized(field, transform) => {
                let v = fields.get(field)?;
                let normalized = transform.apply(&crate::ops::put::value_to_anchor_string(v));
                Some(vec![(field.clone(), Value::Text(normalized))])
            }
            GravitySpec::Composite(components) => {
                let mut pairs = Vec::with_capacity(components.len());
                for field in components {
                    // Every component is required — a partial tuple cannot pin
                    // the bucket (it would hash to a different value).
                    pairs.push((field.clone(), fields.get(field)?.clone()));
                }
                Some(pairs)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::put::compute_gravity_hash;
    use xyzdb_core::key::hash_to_48bits;

    fn fields(pairs: &[(&str, Value)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// D1: `Raw` folds the VALUE ONLY (the field name is not part of the hash
    /// input) — the single canonical convention shared with the anchor/LID
    /// fallback, `LINK`, and `PLACE`. A `*field` row and an anchor-placed row on
    /// the same value now reach the same bucket (closes the asymmetry).
    #[test]
    fn raw_hash_is_value_only_canonical() {
        let f = fields(&[
            ("rfc", Value::Text("ABC123".into())),
            ("monto", Value::Int(50)),
        ]);
        let spec = GravitySpec::Raw("rfc".into());

        // Value-only: hashing "ABC123" alone, the field name "rfc" excluded.
        assert_eq!(spec.compute_hash(&f), Some(hash_to_48bits("ABC123")));
        // …and the shared primitive (write == query route through it) agrees.
        let canonical = compute_gravity_hash(&[("rfc".to_string(), Value::Text("ABC123".into()))]);
        assert_eq!(spec.compute_hash(&f), Some(canonical));
    }

    #[test]
    fn raw_missing_field_returns_none() {
        let f = fields(&[("monto", Value::Int(1))]);
        assert_eq!(GravitySpec::Raw("rfc".into()).compute_hash(&f), None);
    }

    /// Normalized folds the value before hashing: `"Acme"` and `"acme"` collide.
    #[test]
    fn normalized_lower_collapses_case() {
        let upper = fields(&[("empresa", Value::Text("Acme".into()))]);
        let lower = fields(&[("empresa", Value::Text("acme".into()))]);
        let spec = GravitySpec::Normalized("empresa".into(), Transform::Lower);

        assert_eq!(spec.compute_hash(&upper), spec.compute_hash(&lower));
        // …and it differs from the un-normalized hash (the transform is real).
        let raw = GravitySpec::Raw("empresa".into());
        assert_ne!(spec.compute_hash(&upper), raw.compute_hash(&upper));
    }

    #[test]
    fn normalized_trim_strips_whitespace() {
        let padded = fields(&[("k", Value::Text("  x  ".into()))]);
        let clean = fields(&[("k", Value::Text("x".into()))]);
        let spec = GravitySpec::Normalized("k".into(), Transform::Trim);
        assert_eq!(spec.compute_hash(&padded), spec.compute_hash(&clean));
    }

    /// Composite folds the full tuple (killing the footgun) and is order- and
    /// completeness-sensitive.
    #[test]
    fn composite_folds_full_tuple_and_is_order_sensitive() {
        let f = fields(&[
            ("tenant", Value::Text("t1".into())),
            ("doc", Value::Text("d1".into())),
        ]);
        let ab = GravitySpec::Composite(vec!["tenant".into(), "doc".into()]);
        let ba = GravitySpec::Composite(vec!["doc".into(), "tenant".into()]);

        // D1 value-only: the tuple folds VALUES joined by `\0`, names excluded.
        assert_eq!(ab.compute_hash(&f), Some(hash_to_48bits("t1\0d1")));
        // …matching the shared primitive WRITE and QUERY both route through.
        let expected = compute_gravity_hash(&[
            ("tenant".to_string(), Value::Text("t1".into())),
            ("doc".to_string(), Value::Text("d1".into())),
        ]);
        assert_eq!(ab.compute_hash(&f), Some(expected));
        assert_ne!(
            ab.compute_hash(&f),
            ba.compute_hash(&f),
            "tuple order matters"
        );
    }

    #[test]
    fn composite_partial_tuple_returns_none() {
        let f = fields(&[("tenant", Value::Text("t1".into()))]); // missing `doc`
        let spec = GravitySpec::Composite(vec!["tenant".into(), "doc".into()]);
        assert_eq!(spec.compute_hash(&f), None);
    }

    #[test]
    fn legacy_field_string_is_raw() {
        assert_eq!(
            GravitySpec::from_legacy_field("rfc"),
            GravitySpec::Raw("rfc".into())
        );
    }

    #[test]
    fn encode_decode_roundtrip() {
        for spec in [
            GravitySpec::Raw("rfc".into()),
            GravitySpec::Normalized("empresa".into(), Transform::Lower),
            GravitySpec::Composite(vec!["tenant".into(), "doc".into()]),
        ] {
            let bytes = spec.encode().unwrap();
            assert_eq!(GravitySpec::decode(&bytes), Some(spec));
        }
    }

    /// Zero-migration: a pre-0.8 slot (`[MAGIC][0x01][postcard(String)]`) decodes
    /// to `Raw(field)` — built exactly as the engine wrote it.
    #[test]
    fn legacy_0x01_slot_decodes_as_raw() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(0x01);
        bytes.extend_from_slice(&postcard::to_allocvec(&"rfc".to_string()).unwrap());
        assert_eq!(
            GravitySpec::decode(&bytes),
            Some(GravitySpec::Raw("rfc".into()))
        );
    }

    /// D1 convention marker: a fresh `encode` (0x03) is NOT pre-D1, while a
    /// Fase-0 (0x02) or pre-Fase-0 (0x01) slot IS — and all three decode to the
    /// same spec. This is what `migrate`'s guard keys on.
    #[test]
    fn slot_convention_marker() {
        let spec = GravitySpec::Raw("rfc".into());
        let current = spec.encode().unwrap();
        assert!(!GravitySpec::slot_is_pre_d1(&current), "0x03 is value-only");
        assert_eq!(GravitySpec::decode(&current), Some(spec.clone()));

        // Hand-build a Fase-0 (0x02) slot: postcard(GravitySpec) behind 0x02.
        let mut v02 = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        v02.push(0x02);
        v02.extend_from_slice(&postcard::to_allocvec(&spec).unwrap());
        assert!(GravitySpec::slot_is_pre_d1(&v02), "0x02 is name+value era");
        assert_eq!(
            GravitySpec::decode(&v02),
            Some(spec),
            "still decodes to the spec"
        );

        // Pre-Fase-0 (0x01) bare field name.
        let mut v01 = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        v01.push(0x01);
        v01.extend_from_slice(&postcard::to_allocvec(&"rfc".to_string()).unwrap());
        assert!(GravitySpec::slot_is_pre_d1(&v01), "0x01 is name+value era");
    }

    #[test]
    fn decode_rejects_bad_magic_or_format() {
        assert_eq!(GravitySpec::decode(b"\x00\x00\x02xx"), None); // wrong magic
        let mut bad_fmt = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        bad_fmt.push(0x09); // unknown format byte
        bad_fmt.extend_from_slice(b"junk");
        assert_eq!(GravitySpec::decode(&bad_fmt), None);
    }

    #[test]
    fn fields_lists_components() {
        assert_eq!(GravitySpec::Raw("a".into()).fields(), vec!["a"]);
        assert_eq!(
            GravitySpec::Composite(vec!["x".into(), "y".into()]).fields(),
            vec!["x", "y"]
        );
    }

    /// Query side, `Raw`: a single `Eq` on the gravity field pins the bucket to
    /// the SAME value-only hash the WRITE side placed it under — write and query
    /// route through one primitive, so they cannot diverge.
    #[test]
    fn pinned_hash_raw_matches_write_side() {
        use xyzdb_core::record::FilterOp;
        let spec = GravitySpec::Raw("rfc".into());
        let filters = vec![
            ("rfc".to_string(), FilterOp::Eq, Value::Text("ABC".into())),
            ("monto".to_string(), FilterOp::Gt, Value::Int(0)), // non-gravity → ignored
        ];
        // Value-only and identical to the write-side hash for the same value.
        assert_eq!(
            spec.pinned_gravity_hash(&filters),
            Some(hash_to_48bits("ABC"))
        );
    }

    #[test]
    fn pinned_hash_none_when_absent_non_eq_or_duplicated() {
        use xyzdb_core::record::FilterOp;
        let spec = GravitySpec::Raw("rfc".into());
        assert_eq!(
            spec.pinned_gravity_hash(&[("monto".into(), FilterOp::Eq, Value::Int(1))]),
            None,
            "gravity field absent → no fast path"
        );
        assert_eq!(
            spec.pinned_gravity_hash(&[("rfc".into(), FilterOp::Gt, Value::Text("A".into()))]),
            None,
            "range on gravity field → no fast path"
        );
        assert_eq!(
            spec.pinned_gravity_hash(&[
                ("rfc".into(), FilterOp::Eq, Value::Text("A".into())),
                ("rfc".into(), FilterOp::Eq, Value::Text("B".into())),
            ]),
            None,
            "duplicate Eq on gravity field → bail"
        );
    }

    /// `WHERE empresa = "Acme"` must pin to the bucket of stored `"acme"`.
    #[test]
    fn pinned_hash_normalized_folds_query_value() {
        use xyzdb_core::record::FilterOp;
        let spec = GravitySpec::Normalized("empresa".into(), Transform::Lower);
        let query = spec.pinned_gravity_hash(&[(
            "empresa".into(),
            FilterOp::Eq,
            Value::Text("Acme".into()),
        )]);
        let stored = spec.compute_hash(&fields(&[("empresa", Value::Text("acme".into()))]));
        assert_eq!(query, stored);
    }

    #[test]
    fn pinned_hash_composite_requires_full_tuple() {
        use xyzdb_core::record::FilterOp;
        let spec = GravitySpec::Composite(vec!["tenant".into(), "doc".into()]);
        let full = vec![
            ("tenant".to_string(), FilterOp::Eq, Value::Text("t1".into())),
            ("doc".to_string(), FilterOp::Eq, Value::Text("d1".into())),
        ];
        assert_eq!(
            spec.pinned_gravity_hash(&full),
            spec.compute_hash(&fields(&[
                ("tenant", Value::Text("t1".into())),
                ("doc", Value::Text("d1".into())),
            ]))
        );
        assert_eq!(
            spec.pinned_gravity_hash(&[("tenant".into(), FilterOp::Eq, Value::Text("t1".into()))]),
            None,
            "partial tuple → no fast path"
        );
    }
}
