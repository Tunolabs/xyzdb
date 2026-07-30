use std::collections::HashMap;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::field_dict::FieldDict;
use xyzdb_core::record::{Record, XYZDB_MAGIC};

/// Dictionary keyspace prefix for field registries: `[FIELD_REGISTRY][lobe_id:2]`.
/// Canonical definition and the non-collision invariant live in
/// [`crate::reserved_keys`]. The value shape is identical to pinned fields, so
/// these two keyspaces must never share a prefix (enforced there).
const FIELD_REGISTRY_PREFIX: [u8; 2] = crate::reserved_keys::FIELD_REGISTRY;

/// Per-lobe field name ↔ u16 registry backing the V2 on-disk format.
///
/// V2 records (`xyzdb_core::record::serialize_record_v2`) store field NAMES as
/// compact u16 IDs and depend on this registry to decode them on read. The
/// id→name mapping for a lobe is persisted into the dictionary keyspace as a
/// single `[0xFF][0xFB][lobe_id:2]` entry, and is committed ATOMICALLY with the
/// record that introduces a new field name (see `serialize_record_v2_durable`).
/// That co-commit is what keeps an acknowledged V2 record decodable after a
/// crash: the mapping can never lag behind — or get ahead of — the record that
/// uses it. There is no deferred / dirty-flushed persistence path.
#[derive(Default)]
pub struct LobeFieldRegistry {
    dicts: HashMap<u16, FieldDict>,
}

impl LobeFieldRegistry {
    pub fn new() -> Self {
        Self {
            dicts: HashMap::new(),
        }
    }

    /// Get the FieldDict for a lobe (read-only). Returns None if no fields registered.
    pub fn get_dict(&self, lobe_id: u16) -> Option<&FieldDict> {
        self.dicts.get(&lobe_id)
    }

    /// Serialize `record` as V2 for `lobe_id`, registering any new field names.
    ///
    /// Returns the V2 bytes and, when the lobe's dictionary GREW, the
    /// `(dictionary key, value)` pair the caller MUST write in the SAME
    /// `turba.batch()` as the record. Committing the id→name mapping atomically
    /// with the record that depends on it is the durability invariant: an acked
    /// V2 record is always decodable after a crash because its mapping landed in
    /// the same WAL commit. When the dict did not grow (no new names) the
    /// mapping already on disk is still valid, so no dictionary write is needed.
    ///
    /// # Errors
    ///
    /// Returns [`XyzError::Storage`] if the field-name list fails to serialize.
    // Return tuple mirrors the co-commit contract; a type alias is a design change, deferred.
    #[allow(clippy::type_complexity)]
    pub fn serialize_record_v2_durable(
        &mut self,
        lobe_id: u16,
        record: &Record,
    ) -> Result<(Vec<u8>, Option<(Vec<u8>, Vec<u8>)>)> {
        let dict = self.dicts.entry(lobe_id).or_default();
        let before = dict.len();
        let bytes = xyzdb_core::record::serialize_record_v2(record, dict);

        let entry = if dict.len() > before {
            Some((
                Self::registry_key(lobe_id).to_vec(),
                Self::encode_dict(dict)?,
            ))
        } else {
            None
        };

        Ok((bytes, entry))
    }

    /// Serialize `record` as V5 for `lobe_id` — the split layout where the
    /// searchable vector is hoisted OUT of the blob and into the `vectors`
    /// keyspace as a separate column entry. Returns the blob (no vector), the
    /// `Option<column_value>` (present iff `search_field` names a stored
    /// `Value::Vector`), and the dictionary-growth entry.
    ///
    /// The column is a V4-shaped mini-blob parseable by
    /// `read_vector_prefix_raw_norm`, so NEAREST scores it unchanged. The caller
    /// MUST write the column under the record's spatial key (the SAME key as the
    /// blob) and co-commit the dictionary `(key, value)` pair, when present, in
    /// the same `turba.batch()` as the record — the field-id→name mapping is
    /// durable iff the record is.
    ///
    /// # Errors
    ///
    /// Returns [`XyzError::Storage`] if the field-name list fails to serialize.
    // Return tuple mirrors the co-commit contract; a type alias is a design change, deferred.
    #[allow(clippy::type_complexity)]
    pub fn serialize_record_v5_durable(
        &mut self,
        lobe_id: u16,
        record: &Record,
        search_field: Option<&str>,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>, Option<(Vec<u8>, Vec<u8>)>)> {
        let dict = self.dicts.entry(lobe_id).or_default();
        let before = dict.len();
        let (blob, column) = xyzdb_core::record::serialize_record_v5(record, dict, search_field);

        let entry = if dict.len() > before {
            Some((
                Self::registry_key(lobe_id).to_vec(),
                Self::encode_dict(dict)?,
            ))
        } else {
            None
        };

        Ok((blob, column, entry))
    }

    /// Encode a dict's names for the dictionary keyspace:
    /// `[MAGIC:2][0x01][postcard(Vec<String>)]`.
    fn encode_dict(dict: &FieldDict) -> Result<Vec<u8>> {
        let names = dict.to_names().to_vec();
        let payload = postcard::to_allocvec(&names)
            .map_err(|e| XyzError::Storage(format!("field registry serialize: {e}")))?;

        let mut val = Vec::with_capacity(3 + payload.len());
        val.extend_from_slice(&XYZDB_MAGIC);
        val.push(0x01);
        val.extend_from_slice(&payload);
        Ok(val)
    }

    /// Load all field registries from the dictionary keyspace at boot.
    pub fn load_from_disk(dictionary: &turba_engine::tree::Tree) -> Self {
        let mut registry = Self::new();

        let entries = match dictionary.prefix(&FIELD_REGISTRY_PREFIX) {
            Ok(e) => e,
            Err(_) => return registry,
        };
        for entry in entries {
            let key_bytes = &entry.key;
            let val_bytes = &entry.value;

            // Key: [0xFF][0xFB][lobe_id:2]
            if key_bytes.len() < 4 {
                continue;
            }
            let lobe_id = u16::from_be_bytes([key_bytes[2], key_bytes[3]]);

            // Value: try postcard with magic prefix, fallback to raw postcard
            let names: Vec<String> =
                if val_bytes.len() >= 3 && val_bytes[0..2] == XYZDB_MAGIC && val_bytes[2] == 0x01 {
                    match postcard::from_bytes(&val_bytes[3..]) {
                        Ok(n) => n,
                        Err(_) => continue,
                    }
                } else {
                    // No legacy format for field registries (new in V5 Fase 2)
                    continue;
                };

            if !names.is_empty() {
                tracing::info!(
                    "Loaded field registry for lobe {}: {} fields",
                    lobe_id,
                    names.len()
                );
                registry.dicts.insert(lobe_id, FieldDict::from_names(names));
            }
        }

        registry
    }

    fn registry_key(lobe_id: u16) -> [u8; 4] {
        let mut key = [0u8; 4];
        key[0..2].copy_from_slice(&FIELD_REGISTRY_PREFIX);
        key[2..4].copy_from_slice(&lobe_id.to_be_bytes());
        key
    }
}
