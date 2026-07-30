use std::collections::HashMap;

/// Bidirectional field name ↔ u16 ID mapping for a single lobe.
/// Used by V2 on-disk format to replace string field names with compact IDs.
/// Pure data structure — no persistence logic (that lives in engine::field_registry).
pub struct FieldDict {
    name_to_id: HashMap<String, u16>,
    id_to_name: Vec<String>, // index = field_id
}

impl FieldDict {
    pub fn new() -> Self {
        Self {
            name_to_id: HashMap::new(),
            id_to_name: Vec::new(),
        }
    }

    /// Reconstruct from a persisted list of names (ordered by field_id).
    pub fn from_names(names: Vec<String>) -> Self {
        let mut name_to_id = HashMap::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            name_to_id.insert(name.clone(), i as u16);
        }
        Self {
            name_to_id,
            id_to_name: names,
        }
    }

    /// Get or assign a field ID. Returns (id, true) if newly created.
    pub fn get_or_create_id(&mut self, name: &str) -> (u16, bool) {
        if let Some(&id) = self.name_to_id.get(name) {
            return (id, false);
        }
        let id = self.id_to_name.len() as u16;
        self.id_to_name.push(name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        (id, true)
    }

    /// Lookup name by ID. Returns None if ID is out of range (corruption).
    pub fn get_name(&self, id: u16) -> Option<&str> {
        self.id_to_name.get(id as usize).map(|s| s.as_str())
    }

    /// Export names for persistence (ordered by field_id).
    pub fn to_names(&self) -> &[String] {
        &self.id_to_name
    }

    /// Number of registered fields.
    pub fn len(&self) -> usize {
        self.id_to_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_name.is_empty()
    }
}

impl Default for FieldDict {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_ids() {
        let mut d = FieldDict::new();
        let (id0, new0) = d.get_or_create_id("name");
        let (id1, new1) = d.get_or_create_id("age");
        let (id0b, new0b) = d.get_or_create_id("name");
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id0b, 0);
        assert!(new0);
        assert!(new1);
        assert!(!new0b);
    }

    #[test]
    fn roundtrip_from_names() {
        let mut d = FieldDict::new();
        d.get_or_create_id("alpha");
        d.get_or_create_id("beta");
        d.get_or_create_id("gamma");

        let names = d.to_names().to_vec();
        let restored = FieldDict::from_names(names);
        assert_eq!(restored.get_name(0), Some("alpha"));
        assert_eq!(restored.get_name(1), Some("beta"));
        assert_eq!(restored.get_name(2), Some("gamma"));
        assert_eq!(restored.get_name(3), None);
    }

    #[test]
    fn get_name_out_of_range() {
        let d = FieldDict::new();
        assert_eq!(d.get_name(0), None);
        assert_eq!(d.get_name(999), None);
    }
}
