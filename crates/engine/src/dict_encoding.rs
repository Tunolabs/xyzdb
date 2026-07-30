use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::value::Value;

/// Reserved dictionary prefix for dict encoding: `[DICT][lobe_id:2]…` (keys are
/// always longer than the bare prefix, which length-disambiguates them from the
/// boot-epoch key that shares it). See [`crate::reserved_keys`].
const DICT_PREFIX: [u8; 2] = crate::reserved_keys::DICT;

/// Bidirectional codec for a single (lobe, field) pair.
#[derive(Clone)]
struct DictCodec {
    // parked: dict-compression
    #[allow(dead_code)]
    encode: HashMap<String, u16>,
    decode: Vec<String>,
}

impl DictCodec {
    fn from_values(values: Vec<String>) -> Self {
        let mut encode = HashMap::with_capacity(values.len());
        for (i, v) in values.iter().enumerate() {
            encode.insert(v.clone(), i as u16);
        }
        Self {
            encode,
            decode: values,
        }
    }

    // parked: dict-compression
    #[allow(dead_code)]
    fn encode_value(&self, s: &str) -> Option<u16> {
        self.encode.get(s).copied()
    }

    // parked: dict-compression
    #[allow(dead_code)]
    fn decode_value(&self, code: u16) -> Option<&str> {
        self.decode.get(code as usize).map(|s| s.as_str())
    }
}

/// Persisted dictionary: just the ordered list of values.
#[derive(Serialize, Deserialize)]
struct PersistedDict {
    values: Vec<String>,
}

/// Stores dictionary codecs per (lobe, field). Loaded at boot, updated by ANALYZE.
pub struct DictRegistry {
    codecs: HashMap<(String, String), DictCodec>,
}

impl Default for DictRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DictRegistry {
    pub fn new() -> Self {
        Self {
            codecs: HashMap::new(),
        }
    }

    /// Register a dictionary for (lobe, field) with the given unique values.
    pub fn register(&mut self, lobe: &str, field: &str, values: Vec<String>) {
        let codec = DictCodec::from_values(values);
        self.codecs
            .insert((lobe.to_string(), field.to_string()), codec);
    }

    /// Check if a dictionary exists for (lobe, field).
    // parked: dict-compression
    #[allow(dead_code)]
    pub fn has_encoding(&self, lobe: &str, field: &str) -> bool {
        self.codecs
            .contains_key(&(lobe.to_string(), field.to_string()))
    }

    /// Encode a text value to its dictionary code. Returns None if no encoding or value unknown.
    // parked: dict-compression
    #[allow(dead_code)]
    pub fn encode(&self, lobe: &str, field: &str, value: &str) -> Option<u16> {
        self.codecs
            .get(&(lobe.to_string(), field.to_string()))
            .and_then(|c| c.encode_value(value))
    }

    /// Decode a dictionary code back to its text value.
    // parked: dict-compression
    #[allow(dead_code)]
    pub fn decode(&self, lobe: &str, field: &str, code: u16) -> Option<String> {
        self.codecs
            .get(&(lobe.to_string(), field.to_string()))
            .and_then(|c| c.decode_value(code))
            .map(|s| s.to_string())
    }

    /// Get all dict-encoded field names for a lobe.
    // parked: dict-compression
    #[allow(dead_code)]
    pub fn encoded_fields(&self, lobe: &str) -> Vec<String> {
        self.codecs
            .keys()
            .filter(|(l, _)| l == lobe)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// Encode fields in a record's BTreeMap for ghost storage.
    /// Returns the list of field names that were dict-encoded.
    // parked: dict-compression
    #[allow(dead_code)]
    pub fn encode_record_fields(
        &self,
        lobe: &str,
        fields: &mut std::collections::BTreeMap<String, Value>,
    ) -> Vec<String> {
        let mut encoded = Vec::new();
        for (field_name, value) in fields.iter_mut() {
            if let Value::Text(s) = value
                && let Some(code) = self.encode(lobe, field_name, s)
            {
                *value = Value::Int(code as i64);
                encoded.push(field_name.clone());
            }
        }
        encoded
    }

    /// Decode dict-encoded fields in a record back to their original text values.
    // parked: dict-compression
    #[allow(dead_code)]
    pub fn decode_record_fields(
        &self,
        lobe: &str,
        fields: &mut std::collections::BTreeMap<String, Value>,
        dict_encoded_fields: &[String],
    ) {
        for field_name in dict_encoded_fields {
            if let Some(Value::Int(code)) = fields.get(field_name)
                && let Some(original) = self.decode(lobe, field_name, *code as u16)
            {
                fields.insert(field_name.clone(), Value::Text(original));
            }
        }
    }

    // ── Persistence ─────────────────────────────────────────────────────

    fn dict_key(lobe_id: u16, field: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(4 + 1 + field.len());
        key.extend_from_slice(&DICT_PREFIX);
        key.extend_from_slice(&lobe_id.to_be_bytes());
        key.push(field.len() as u8);
        key.extend_from_slice(field.as_bytes());
        key
    }

    /// Persist a dictionary to the dictionary keyspace.
    pub fn persist(
        &self,
        dictionary: &turba_engine::tree::Tree,
        lobe_id: u16,
        lobe: &str,
        field: &str,
    ) -> Result<()> {
        if let Some(codec) = self.codecs.get(&(lobe.to_string(), field.to_string())) {
            let persisted = PersistedDict {
                values: codec.decode.clone(),
            };
            let payload = postcard::to_allocvec(&persisted)
                .map_err(|e| XyzError::Storage(format!("dict encoding serialize: {e}")))?;
            let mut bytes = Vec::with_capacity(3 + payload.len());
            bytes.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
            bytes.push(0x01);
            bytes.extend_from_slice(&payload);
            let key = Self::dict_key(lobe_id, field);
            dictionary
                .insert(&key, &bytes)
                .map_err(|e| XyzError::Storage(format!("dict encoding persist: {e}")))?;
        }
        Ok(())
    }

    /// Load all dictionaries from the dictionary keyspace for all lobes.
    pub fn load_all(
        dictionary: &turba_engine::tree::Tree,
        lobes: &xyzdb_core::lobe::LobeRegistry,
    ) -> Self {
        let mut store = Self::new();

        for (lobe_name, config) in lobes.all() {
            let mut prefix = Vec::with_capacity(4);
            prefix.extend_from_slice(&DICT_PREFIX);
            prefix.extend_from_slice(&config.id.to_be_bytes());

            let entries = match dictionary.prefix(&prefix) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries {
                let key_bytes = &entry.key;
                let val_bytes = &entry.value;

                // Parse field name from key: [0xFF][0xFC][lobe_id:2][field_len:1][field_bytes]
                if key_bytes.len() < 5 {
                    continue;
                }
                let field_len = key_bytes[4] as usize;
                if key_bytes.len() < 5 + field_len {
                    continue;
                }
                let field_name = match std::str::from_utf8(&key_bytes[5..5 + field_len]) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };

                let persisted_opt: Option<PersistedDict> = if val_bytes.len() >= 3
                    && val_bytes[0..2] == xyzdb_core::record::XYZDB_MAGIC
                    && val_bytes[2] == 0x01
                {
                    postcard::from_bytes(&val_bytes[3..])
                        .ok()
                        .or_else(|| bincode::deserialize(val_bytes).ok())
                } else {
                    bincode::deserialize(val_bytes).ok()
                };
                if let Some(persisted) = persisted_opt
                    && !persisted.values.is_empty()
                {
                    store.register(lobe_name, &field_name, persisted.values);
                    tracing::info!(
                        "Loaded dict encoding for '{}'.'{}'  ({} values)",
                        lobe_name,
                        field_name,
                        store.codecs[&(lobe_name.to_string(), field_name.clone())]
                            .decode
                            .len()
                    );
                }
            }
        }

        store
    }
}
