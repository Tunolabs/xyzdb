//! Zone map builder and evaluator for per-block data skipping.
//!
//! The builder deserializes record values from SSTable entries and constructs
//! per-field min/max metadata for each data block. During scans, the evaluator
//! checks if a block might contain records matching the query filters —
//! if not, the block is skipped entirely (no decompression, no I/O).

// SPDX-License-Identifier: BUSL-1.1
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use turba_engine::table::writer::ZoneMapBuilder;
use turba_engine::types::Entry;
use xyzdb_core::value::Value;

/// Per-field min/max within a single data block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldZone {
    pub field: String,
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub has_null: bool,
}

/// Zone map for a single data block: min/max per field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockZoneMap {
    pub fields: Vec<FieldZone>,
}

/// Builder that xyzdb-engine passes to turba's SSTableWriter.
/// Deserializes record values and tracks min/max per field.
pub struct XyzZoneMapBuilder {
    /// Top fields to track. Empty = track all fields seen.
    pub tracked_fields: Vec<String>,
    pub source_lobe: String,
}

impl ZoneMapBuilder for XyzZoneMapBuilder {
    fn build_block_zone_map(&self, entries: &[Entry]) -> Vec<u8> {
        let mut field_mins: BTreeMap<String, Value> = BTreeMap::new();
        let mut field_maxs: BTreeMap<String, Value> = BTreeMap::new();
        let mut field_nulls: BTreeMap<String, bool> = BTreeMap::new();

        for entry in entries {
            // Deserialize the record value to extract fields
            let record =
                match xyzdb_core::record::deserialize_record(&entry.value, &self.source_lobe, None)
                {
                    Ok(r) => r,
                    Err(_) => continue,
                };

            for (field_name, value) in &record.fields {
                // If tracked_fields is set, only track those
                if !self.tracked_fields.is_empty()
                    && !self.tracked_fields.iter().any(|f| f == field_name)
                {
                    continue;
                }

                if matches!(value, Value::Null) {
                    field_nulls.insert(field_name.clone(), true);
                    continue;
                }

                // Update min
                let update_min = match field_mins.get(field_name) {
                    None => true,
                    Some(current) => value_cmp(value, current).is_lt(),
                };
                if update_min {
                    field_mins.insert(field_name.clone(), value.clone());
                }

                // Update max
                let update_max = match field_maxs.get(field_name) {
                    None => true,
                    Some(current) => value_cmp(value, current).is_gt(),
                };
                if update_max {
                    field_maxs.insert(field_name.clone(), value.clone());
                }
            }
        }

        // Build the zone map struct
        let mut fields = Vec::new();
        let all_field_names: Vec<String> = field_mins
            .keys()
            .chain(field_nulls.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        for name in all_field_names {
            fields.push(FieldZone {
                min: field_mins.get(&name).cloned(),
                max: field_maxs.get(&name).cloned(),
                has_null: *field_nulls.get(&name).unwrap_or(&false),
                field: name,
            });
        }

        let zm = BlockZoneMap { fields };
        postcard::to_allocvec(&zm).unwrap_or_default()
    }
}

/// Evaluate if a block zone map might match the given filters.
/// Returns false = definitely skip this block. True = might contain matches.
// parked: zone-map reader
#[allow(dead_code)]
pub fn might_match(
    zone_map_bytes: &[u8],
    filters: &[(String, xyzdb_core::record::FilterOp, Value)],
) -> bool {
    let zm: BlockZoneMap = match postcard::from_bytes(zone_map_bytes) {
        Ok(z) => z,
        Err(_) => return true, // Can't parse → don't skip
    };

    for (field_name, op, filter_value) in filters {
        let fz = match zm.fields.iter().find(|f| f.field == *field_name) {
            Some(f) => f,
            None => continue, // Field not tracked in this block → can't skip
        };

        use xyzdb_core::record::FilterOp;
        match op {
            FilterOp::Eq => {
                // Block can be skipped if filter_value < min OR filter_value > max
                if let (Some(min), Some(max)) = (&fz.min, &fz.max)
                    && (value_cmp(filter_value, min).is_lt()
                        || value_cmp(filter_value, max).is_gt())
                {
                    return false;
                }
            }
            FilterOp::Gt | FilterOp::Gte => {
                // filter_value > X: skip if block's max < filter_value (Gt) or max <= filter_value (nothing, we're conservative)
                if let Some(ref max) = fz.max
                    && value_cmp(max, filter_value).is_lt()
                {
                    return false;
                }
            }
            FilterOp::Lt | FilterOp::Lte => {
                if let Some(ref min) = fz.min
                    && value_cmp(min, filter_value).is_gt()
                {
                    return false;
                }
            }
            _ => {} // IsNull, IsNotNull, Contains, Neq — can't skip with min/max
        }
    }

    true // All filters passed → block might contain matches
}

/// Compare two Values for ordering. Only works for comparable types.
fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal, // Incomparable types → treat as equal
    }
}
