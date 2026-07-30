use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Configuration of a single lobe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobeConfig {
    pub id: u16,
    pub name: String,
    pub hint: Option<String>,
    pub created_at: i64, // microseconds since epoch
}

/// Bidirectional mapping name <-> id for all lobes.
/// Persisted to disk as bincode in the meta/ directory.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LobeRegistry {
    by_name: BTreeMap<String, LobeConfig>,
    next_id: u16,
}

impl LobeRegistry {
    pub fn new() -> Self {
        Self {
            by_name: BTreeMap::new(),
            next_id: 1,
        }
    }

    /// Get a lobe by name. Returns None if it doesn't exist.
    pub fn get(&self, name: &str) -> Option<&LobeConfig> {
        self.by_name.get(name)
    }

    /// Get a lobe by numeric ID.
    pub fn get_by_id(&self, id: u16) -> Option<&LobeConfig> {
        self.by_name.values().find(|c| c.id == id)
    }

    /// Get or create a lobe. Returns the lobe_id.
    pub fn get_or_create(&mut self, name: &str, hint: Option<String>) -> u16 {
        if let Some(config) = self.by_name.get(name) {
            return config.id;
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        self.by_name.insert(
            name.to_string(),
            LobeConfig {
                id,
                name: name.to_string(),
                hint,
                created_at: now,
            },
        );

        id
    }

    /// Create a lobe explicitly. Returns error if it already exists.
    pub fn create(&mut self, name: &str, hint: Option<String>) -> crate::error::Result<u16> {
        if self.by_name.contains_key(name) {
            return Err(crate::error::XyzError::InvalidQuery(format!(
                "Lobe '{name}' already exists"
            )));
        }
        Ok(self.get_or_create(name, hint))
    }

    /// List all lobes.
    pub fn list(&self) -> Vec<&LobeConfig> {
        let mut lobes: Vec<_> = self.by_name.values().collect();
        lobes.sort_by_key(|l| l.id);
        lobes
    }

    /// Iterate over all lobes.
    pub fn all(&self) -> impl Iterator<Item = (&str, &LobeConfig)> {
        self.by_name
            .iter()
            .map(|(name, config)| (name.as_str(), config))
    }

    /// Serialize registry to bytes (V5: postcard with magic prefix).
    pub fn to_bytes(&self) -> Vec<u8> {
        let payload = postcard::to_allocvec(self).expect("LobeRegistry serialize");
        let mut buf = Vec::with_capacity(3 + payload.len());
        buf.extend_from_slice(&crate::record::XYZDB_MAGIC);
        buf.push(0x01);
        buf.extend_from_slice(&payload);
        buf
    }

    /// Deserialize registry from bytes (auto-detect postcard V1 or legacy bincode).
    pub fn from_bytes(data: &[u8]) -> crate::error::Result<Self> {
        if data.len() >= 3
            && data[0..2] == crate::record::XYZDB_MAGIC
            && data[2] == 0x01
            && let Ok(reg) = postcard::from_bytes(&data[3..])
        {
            return Ok(reg);
        }
        bincode::deserialize(data).map_err(|e| {
            crate::error::XyzError::Storage(format!("Failed to load lobe registry: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get() {
        let mut reg = LobeRegistry::new();
        let id = reg.get_or_create("workspace", None);
        assert_eq!(id, 1);
        assert_eq!(reg.get("workspace").unwrap().id, 1);
    }

    #[test]
    fn get_or_create_idempotent() {
        let mut reg = LobeRegistry::new();
        let id1 = reg.get_or_create("workspace", None);
        let id2 = reg.get_or_create("workspace", None);
        assert_eq!(id1, id2);
    }

    #[test]
    fn increments_id() {
        let mut reg = LobeRegistry::new();
        let a = reg.get_or_create("a", None);
        let b = reg.get_or_create("b", None);
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    #[test]
    fn serialization_roundtrip() {
        let mut reg = LobeRegistry::new();
        reg.get_or_create("test", Some("hint".into()));
        let bytes = reg.to_bytes();
        let restored = LobeRegistry::from_bytes(&bytes).unwrap();
        assert_eq!(restored.get("test").unwrap().id, 1);
    }
}
