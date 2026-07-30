use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use xyzdb_core::error::{Result, XyzError};

/// Registry of anchor constraints. Maps lobe_name -> set of anchor field names.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnchorRegistry {
    anchors: BTreeMap<String, BTreeSet<String>>,
}

impl AnchorRegistry {
    /// Register an anchor field for a lobe. Error if already registered.
    pub fn register(&mut self, lobe: &str, field: &str) -> Result<()> {
        let set = self.anchors.entry(lobe.to_string()).or_default();
        if !set.insert(field.to_string()) {
            return Err(XyzError::InvalidQuery(format!(
                "Anchor '{field}' already registered in lobe '{lobe}'"
            )));
        }
        Ok(())
    }

    /// Get all anchor fields for a lobe.
    pub fn get_anchors(&self, lobe: &str) -> &BTreeSet<String> {
        static EMPTY: std::sync::LazyLock<BTreeSet<String>> =
            std::sync::LazyLock::new(BTreeSet::new);
        self.anchors.get(lobe).unwrap_or(&EMPTY)
    }

    /// Check if a field is an anchor in a lobe.
    pub fn is_anchor(&self, lobe: &str, field: &str) -> bool {
        self.anchors.get(lobe).is_some_and(|s| s.contains(field))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let payload = postcard::to_allocvec(self).expect("AnchorRegistry serialize");
        let mut buf = Vec::with_capacity(3 + payload.len());
        buf.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        buf.push(0x01);
        buf.extend_from_slice(&payload);
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() >= 3
            && data[0..2] == xyzdb_core::record::XYZDB_MAGIC
            && data[2] == 0x01
            && let Ok(reg) = postcard::from_bytes(&data[3..])
        {
            return Ok(reg);
        }
        bincode::deserialize(data)
            .map_err(|e| XyzError::Storage(format!("Failed to load anchor registry: {e}")))
    }
}

/// Build a dictionary key for an anchor lookup.
/// Format: [lobe_id: u16 BE][field_len: u8][field: bytes][value: bytes]
/// Variable length — zero collision probability.
/// Previous format used FNV hash (18 bytes fixed) but had birthday-paradox
/// collisions at ~10K values causing silent data loss.
pub fn dictionary_key(lobe_id: u16, field: &str, value: &str) -> Vec<u8> {
    let field_bytes = field.as_bytes();
    let value_bytes = value.as_bytes();
    let field_len = field_bytes.len().min(255) as u8;

    let mut key = Vec::with_capacity(2 + 1 + field_bytes.len() + value_bytes.len());
    key.extend_from_slice(&lobe_id.to_be_bytes());
    key.push(field_len);
    key.extend_from_slice(&field_bytes[..field_len as usize]);
    key.extend_from_slice(value_bytes);
    key
}
