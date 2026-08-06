// SPDX-License-Identifier: BUSL-1.1
use crate::engine::{Engine, QueryResult};
use xytalk_parser::ast::PullStmt;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::{SPATIAL_KEY_SIZE, SpatialKey};
use xyzdb_core::record::Record;

/// Execute PULL: range scan all records sharing the same gravity_hash prefix.
const MAX_PULL_DEPTH: u32 = 10;

pub fn execute_pull(
    engine: &Engine,
    mut stmt: PullStmt,
    source_records: Option<Vec<Record>>,
) -> Result<QueryResult> {
    stmt.depth = stmt.depth.min(MAX_PULL_DEPTH);
    let records = match source_records {
        Some(recs) => recs,
        None => {
            // Standalone PULL FROM <target>
            let target = stmt.target.as_ref().ok_or_else(|| {
                XyzError::InvalidQuery("PULL requires a target or pipeline".into())
            })?;
            let found = engine.resolve_find(target, &[])?;
            found.into_iter().map(|(r, _)| r).collect()
        }
    };

    if records.is_empty() {
        return Ok(QueryResult::Records(vec![]));
    }

    // Collect gravity_hashes from source records via Identity lookup
    let mut seen_entities = std::collections::HashSet::new();
    let mut all_results = Vec::new();

    for record in &records {
        let lid_bytes = record.lid.to_bytes();
        let spatial_key_val = match engine
            .turba
            .identity
            .get(&lid_bytes)
            .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
        {
            Some(v) => v,
            None => continue,
        };

        let sk_bytes: [u8; SPATIAL_KEY_SIZE] = if spatial_key_val.len() == SPATIAL_KEY_SIZE {
            let mut arr = [0u8; SPATIAL_KEY_SIZE];
            arr.copy_from_slice(&spatial_key_val);
            arr
        } else if spatial_key_val.len() == 10 {
            // Legacy 10-byte key: pad with seq=0
            let mut arr = [0u8; SPATIAL_KEY_SIZE];
            arr[..10].copy_from_slice(&spatial_key_val);
            arr
        } else {
            continue;
        };

        let sk = SpatialKey::from_bytes(&sk_bytes);
        let lobe_id = sk.lobe_id;
        let gravity_hash = sk.gravity_hash;

        let pull_lobe_name = engine.lobe_name_for_id(lobe_id);

        // Collision guard inputs: the lobe's registered gravity field and
        // the SOURCE record's canonical value for it. The 48-bit bucket can
        // hold records of a different gravity value that hashed identically;
        // those are discarded below. `None` (source doesn't carry the field,
        // e.g. it is itself a LINK TO child) disables the guard — there is
        // no value to verify against.
        let gravity_field = engine.get_gravity_field(&pull_lobe_name);
        let source_canon: Option<String> = gravity_field
            .as_ref()
            .and_then(|gf| record.fields.get(gf))
            .map(crate::ops::put::value_to_anchor_string);

        // Skip if we've already scanned this entity. The canonical value is
        // part of the key: two source records whose distinct values collide
        // into one bucket must each scan with their own guard.
        if !seen_entities.insert((lobe_id, gravity_hash, source_canon.clone())) {
            continue;
        }

        // Range scan: all records with this entity prefix
        let fr_guard = engine.field_registry.read();
        let fd = fr_guard.get_dict(lobe_id);
        let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, gravity_hash);
        let tree = engine.spatial_tree();
        for entry in tree
            .range_stream(key_min.as_slice(), key_max.as_slice())
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            let val = &entry.value;
            if let Ok(rec) =
                crate::ops::deserialize_hydrated(engine, &entry.key, val, &pull_lobe_name, fd)
            {
                if is_collision_victim(&rec, &gravity_field, &source_canon, gravity_hash) {
                    continue;
                }
                // Apply only= filter if specified
                if let Some(ref only_type) = stmt.only {
                    if let Some(xyzdb_core::value::Value::Text(t)) = rec.fields.get("_type") {
                        if t != only_type {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                all_results.push(rec);
            }
        }
    }

    Ok(QueryResult::Records(all_results))
}

/// True when `rec` provably belongs to a DIFFERENT entity that
/// hash-collided into this bucket: it carries the gravity field with
/// another canonical value AND that value re-hashes to this same bucket
/// (the only way it could have landed here by gravity). A LINK TO child
/// carrying its own gravity value re-hashes elsewhere — it was placed
/// here by the link override, so it is kept (documented LINK semantics).
/// Records without the field are kept: nothing to verify against.
fn is_collision_victim(
    rec: &Record,
    gravity_field: &Option<String>,
    source_canon: &Option<String>,
    bucket_hash: u64,
) -> bool {
    let (Some(gf), Some(src)) = (gravity_field, source_canon) else {
        return false;
    };
    let Some(v) = rec.fields.get(gf) else {
        return false;
    };
    if &crate::ops::put::value_to_anchor_string(v) == src {
        return false;
    }
    crate::ops::put::compute_gravity_hash(&[(gf.clone(), v.clone())]) == bucket_hash
}

#[cfg(test)]
mod collision_victim_tests {
    use super::*;
    use std::collections::BTreeMap;
    use xyzdb_core::lid::LID;
    use xyzdb_core::value::Value;

    fn rec_with(field: &str, value: &str) -> Record {
        let mut fields = BTreeMap::new();
        fields.insert(field.to_string(), Value::Text(value.to_string()));
        Record {
            lid: LID::new(1),
            lobe_name: "l".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// A true collision can't be fabricated from real strings (48-bit
    /// birthday search), but `bucket_hash` is a parameter: scanning "as if"
    /// the bucket were the OTHER value's bucket reproduces the exact
    /// condition the guard must detect.
    #[test]
    fn detects_value_whose_own_hash_owns_the_bucket() {
        let gf = Some("key".to_string());
        let src = Some("K1".to_string());
        let rec = rec_with("key", "K2");
        let k2_bucket =
            crate::ops::put::compute_gravity_hash(&[("key".into(), Value::Text("K2".into()))]);
        assert!(is_collision_victim(&rec, &gf, &src, k2_bucket));
    }

    #[test]
    fn keeps_same_value_members_and_link_overrides() {
        let gf = Some("key".to_string());
        let src = Some("K1".to_string());
        let k1_bucket =
            crate::ops::put::compute_gravity_hash(&[("key".into(), Value::Text("K1".into()))]);

        // Same canonical value → legitimate member.
        assert!(!is_collision_victim(
            &rec_with("key", "K1"),
            &gf,
            &src,
            k1_bucket
        ));
        // Different value whose own hash points elsewhere → LINK TO
        // override placed it here deliberately; keep.
        assert!(!is_collision_victim(
            &rec_with("key", "K2"),
            &gf,
            &src,
            k1_bucket
        ));
        // No gravity field on the record → nothing to verify; keep.
        assert!(!is_collision_victim(
            &rec_with("other", "x"),
            &gf,
            &src,
            k1_bucket
        ));
        // No registered gravity field / no source value → guard disabled.
        assert!(!is_collision_victim(
            &rec_with("key", "K2"),
            &None,
            &src,
            k1_bucket
        ));
        assert!(!is_collision_victim(
            &rec_with("key", "K2"),
            &gf,
            &None,
            k1_bucket
        ));
    }
}
