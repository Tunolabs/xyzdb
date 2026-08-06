// SPDX-License-Identifier: BUSL-1.1
use crate::engine::{Engine, QueryResult};
use std::collections::{HashMap, HashSet};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::record::deserialize_record;
use xyzdb_core::value::Value;

/// Maximum records to sample for ANALYZE.
const SAMPLE_SIZE: usize = 10_000;

/// Cap on distinct TEXT values retained per field for dictionary auto-creation.
/// Sits safely above the `< 1000` low-cardinality threshold a field must meet to
/// qualify, so a qualifying field never loses a value; once a field exceeds this it
/// cannot be low-cardinality, so its set is dropped to bound memory. This collection
/// replaces the old per-field full-lobe re-scan (H14: `prefix()` materialised the
/// entire lobe → RSS burst). Now the unique values come from the SAME sampled records
/// the profile is built from, in a single streaming pass.
const UNIQUE_TEXT_CAP: usize = 2000;

/// Per-field statistics collected during analysis.
struct FieldProfile {
    count: u64,
    value_hashes: HashSet<u64>,
    type_counts: HashMap<&'static str, u64>,
    min_len: Option<usize>,
    max_len: Option<usize>,
    /// Distinct TEXT values seen (for dict auto-creation), or `None` once the field
    /// exceeds `UNIQUE_TEXT_CAP` (too high-cardinality to ever qualify).
    unique_texts: Option<HashSet<String>>,
}

impl FieldProfile {
    fn new() -> Self {
        Self {
            count: 0,
            value_hashes: HashSet::new(),
            type_counts: HashMap::new(),
            min_len: None,
            max_len: None,
            unique_texts: Some(HashSet::new()),
        }
    }

    fn observe(&mut self, value: &Value) {
        self.count += 1;

        let (type_name, hash, len) = match value {
            Value::Text(s) => ("TEXT", hash_bytes(s.as_bytes()), Some(s.len())),
            Value::Int(i) => ("INT", hash_bytes(&i.to_le_bytes()), None),
            Value::Float(f) => ("FLOAT", hash_bytes(&f.to_le_bytes()), None),
            Value::Bool(b) => ("BOOL", hash_bytes(&[*b as u8]), None),
            Value::Timestamp(t) => ("TIMESTAMP", hash_bytes(&t.to_le_bytes()), None),
            Value::Bytes(b) => ("BYTES", hash_bytes(b), Some(b.len())),
            Value::List(_) => ("LIST", 0, None),
            Value::Map(_) => ("MAP", 0, None),
            Value::Null => ("NULL", 0, None),
            Value::Vector(v) => ("VECTOR", 0, Some(v.len())),
        };

        *self.type_counts.entry(type_name).or_insert(0) += 1;
        self.value_hashes.insert(hash);

        // Retain distinct TEXT values (capped) for dictionary auto-creation — the
        // single-pass replacement for the old full-lobe re-scan. Drop the set once it
        // exceeds the cap (can't be low-cardinality, so it would never be used).
        if let Value::Text(s) = value {
            let over_cap = matches!(&self.unique_texts, Some(set) if set.len() >= UNIQUE_TEXT_CAP);
            if over_cap {
                self.unique_texts = None;
            } else if let Some(set) = self.unique_texts.as_mut() {
                set.insert(s.clone());
            }
        }

        if let Some(l) = len {
            self.min_len = Some(self.min_len.map_or(l, |m| m.min(l)));
            self.max_len = Some(self.max_len.map_or(l, |m| m.max(l)));
        }
    }

    fn cardinality(&self) -> u64 {
        self.value_hashes.len() as u64
    }

    fn uniqueness(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.cardinality() as f64 / self.count as f64
    }

    fn dominant_type(&self) -> &'static str {
        self.type_counts
            .iter()
            .max_by_key(|(_, c)| **c)
            .map(|(t, _)| *t)
            .unwrap_or("UNKNOWN")
    }
}

/// Execute ANALYZE: sample records from a lobe and report field statistics.
pub fn execute_analyze(engine: &Engine, lobe_name: &str) -> Result<QueryResult> {
    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(lobe_name)
        .ok_or_else(|| XyzError::LobeNotFound(lobe_name.into()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    let prefix = lobe_id.to_be_bytes();
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let mut profiles: HashMap<String, FieldProfile> = HashMap::new();
    let mut total_records = 0u64;

    for entry in engine
        .turba
        .spatial
        .prefix_iter(&prefix)
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        if total_records >= SAMPLE_SIZE as u64 {
            break;
        }
        let val = &entry.value;
        let record = match deserialize_record(val, lobe_name, fd) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for (field_name, value) in &record.fields {
            if field_name.starts_with('_') {
                continue;
            }
            profiles
                .entry(field_name.clone())
                .or_insert_with(FieldProfile::new)
                .observe(value);
        }

        total_records += 1;
    }

    if total_records == 0 {
        return Ok(QueryResult::Info(vec![format!(
            "ANALYZE \"{lobe_name}\": empty lobe (0 records)"
        )]));
    }

    // V3: Auto-create dictionary encoding for low-cardinality string fields.
    // The distinct values were collected during the single profiling pass above
    // (FieldProfile::unique_texts) from the SAME sampled records — no full-lobe
    // re-scan (H14 fix: the old `prefix()` re-scan materialised the entire lobe
    // into a Vec, per qualifying field, spiking RSS by ~one lobe).
    let mut dict_created = Vec::new();
    if total_records >= 1000 {
        let mut dict_store = engine.dict_store.write();
        for (field_name, profile) in &profiles {
            let cardinality = profile.cardinality();
            if cardinality > 0
                && cardinality < 1000
                && profile.dominant_type() == "TEXT"
                && profile.count >= 100
                && let Some(set) = profile.unique_texts.as_ref()
                && !set.is_empty()
            {
                let mut unique_values: Vec<String> = set.iter().cloned().collect();
                unique_values.sort();
                dict_store.register(lobe_name, field_name, unique_values);
                let _ =
                    dict_store.persist(&engine.turba.dictionary, lobe_id, lobe_name, field_name);
                dict_created.push(field_name.clone());
            }
        }
    }

    // Build report
    let mut lines = vec![
        format!(
            "ANALYZE \"{}\" — {} records sampled",
            lobe_name, total_records
        ),
        String::new(),
    ];

    if !dict_created.is_empty() {
        lines.push(format!(
            "  Dictionary encoding created for: {}",
            dict_created.join(", ")
        ));
        lines.push(String::new());
    }

    // Sort fields by cardinality descending
    let mut fields: Vec<(&String, &FieldProfile)> = profiles.iter().collect();
    fields.sort_by_key(|f| std::cmp::Reverse(f.1.cardinality()));

    for (field_name, profile) in &fields {
        let cardinality = profile.cardinality();
        let uniqueness = profile.uniqueness();
        let dtype = profile.dominant_type();

        let cardinality_label = if uniqueness > 0.99 {
            "HIGH"
        } else if uniqueness > 0.5 {
            "MEDIUM"
        } else if cardinality > 1 {
            "LOW"
        } else {
            "CONSTANT"
        };

        lines.push(format!(
            "  {}: cardinality={} (est. {} unique / {} total), type={}",
            field_name, cardinality_label, cardinality, profile.count, dtype
        ));

        if let (Some(min), Some(max)) = (profile.min_len, profile.max_len) {
            if min == max {
                lines.push(format!("     length: {} bytes", min));
            } else {
                lines.push(format!("     length: {}-{} bytes", min, max));
            }
        }

        // Suggestions
        if uniqueness > 0.99 && profile.count >= 100 {
            lines.push(
                "     -> SUGGESTION: good candidate for ANCHOR (unique identifier)".to_string(),
            );
            lines.push(format!(
                "     -> SUGGESTION: good candidate for *{} gravity tag (co-location key)",
                field_name
            ));
        } else if uniqueness < 0.05 && cardinality <= 20 {
            lines.push(
                "     -> SUGGESTION: good candidate for Ghost Lobe filter (low cardinality enum)"
                    .to_string(),
            );
        } else if uniqueness > 0.5 && uniqueness <= 0.99 {
            lines.push(format!(
                "     -> SUGGESTION: possible *{} gravity tag (shared key across types)",
                field_name
            ));
        }

        lines.push(String::new());
    }

    Ok(QueryResult::Info(lines))
}

fn hash_bytes(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
