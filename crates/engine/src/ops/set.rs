use crate::engine::{Engine, QueryResult};
use crate::ops::literal_to_value;
use xytalk_parser::ast::SetStmt;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::{SPATIAL_KEY_SIZE, SpatialKey, normalize_timestamp};
use xyzdb_core::record::{Record, serialize_record};
use xyzdb_core::value::Value;

/// Execute SET: update fields on matching records.
pub fn execute_set(
    engine: &Engine,
    stmt: SetStmt,
    source_records: Option<Vec<Record>>,
) -> Result<QueryResult> {
    let records = match source_records {
        Some(recs) => recs,
        None => {
            let target = stmt.target.as_ref().ok_or_else(|| {
                XyzError::InvalidQuery("SET requires a target or pipeline".into())
            })?;
            // Standalone SET honours an optional WHERE (xyTalk v1 P1: full
            // OR/NOT/IN tree). AND-pure/no-WHERE takes the anchor/gravity fast
            // path; OR/NOT scans the target + walker-filters.
            let found = engine.resolve_find_expr(target, &stmt.filter_expr)?;
            found.into_iter().map(|(r, _)| r).collect()
        }
    };

    if records.is_empty() {
        return Ok(QueryResult::Ok {
            lid: None,
            message: "0 records updated".into(),
        });
    }

    // Anchors are declared-UNIQUE identity fields. SET does not maintain the
    // anchor dictionary index, so editing an anchor in place would leave the
    // index pointing at the old value — a stale, no-longer-unique index.
    // Reject it up front (before any write): identity is re-created, not edited.
    // Maintaining the index on an anchor change (remove-old + add-new + 2b
    // uniqueness + 1b order) is deferred to the D1/0.8 anchor-key rework.
    {
        let anchors = engine.anchor_registry.read();
        for record in &records {
            for (field, _) in &stmt.assignments {
                if anchors.is_anchor(&record.lobe_name, field) {
                    return Err(XyzError::InvalidQuery(format!(
                        "cannot SET anchor field '{field}' on lobe '{}': anchors are immutable identity",
                        record.lobe_name
                    )));
                }
            }
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;

    let mut count = 0u64;
    for record in &records {
        let lid_bytes = record.lid.to_bytes();
        let sk_val = match engine
            .turba
            .identity
            .get(&lid_bytes)
            .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
        {
            Some(v) => v,
            None => continue,
        };

        let sk_array: [u8; SPATIAL_KEY_SIZE] = if sk_val.len() == SPATIAL_KEY_SIZE {
            sk_val
                .as_slice()
                .try_into()
                .map_err(|_| XyzError::Internal("bad spatial key".into()))?
        } else if sk_val.len() == 10 {
            // Legacy 10-byte key: pad with seq=0
            let mut arr = [0u8; SPATIAL_KEY_SIZE];
            arr[..10].copy_from_slice(&sk_val);
            arr
        } else {
            return Err(XyzError::Internal("bad spatial key".into()));
        };

        let rec_val = match engine
            .turba
            .spatial
            .get(&sk_array)
            .map_err(|e| XyzError::Storage(format!("spatial get: {e}")))?
        {
            Some(v) => v,
            None => continue,
        };

        let lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
        let field_dict_ref = engine.field_registry.read();
        let fd = field_dict_ref.get_dict(lobe_id);
        // Hydrate the existing search vector from the `vectors` column: a V5 blob
        // decodes WITHOUT it, and a SET that does not touch the vector field must
        // not drop it when the record is rewritten (or moved to a new bucket).
        let mut rec =
            crate::ops::deserialize_hydrated(engine, &sk_array, &rec_val, &record.lobe_name, fd)?;
        drop(field_dict_ref);

        // Capture old record for ghost V2 hook before mutation
        let old_record = rec.clone();

        for (k, v) in &stmt.assignments {
            rec.fields.insert(k.clone(), literal_to_value(v));
        }
        rec.updated_at = now;

        // V5 split: serialize the (possibly re-bucketed) record without its
        // vector and emit the vector as a separate column, mirroring the PUT
        // path. A non-vector lobe stays V1. The column follows the record's
        // FINAL spatial key — moved with it on re-gravitation, rewritten in place
        // otherwise — so the `vectors` keyspace tracks `spatial` key-for-key.
        let search = engine.get_vector_spec(&record.lobe_name).map(|s| s.field);
        let (updated, vec_column, vec_reg_entry) = match &search {
            Some(f) if matches!(rec.fields.get(f), Some(Value::Vector(_))) => engine
                .field_registry
                .write()
                .serialize_record_v5_durable(lobe_id, &rec, Some(f))?,
            _ => (serialize_record(&rec), None, None),
        };

        // WAL-durable update. Tree::insert wrote the active memtable only and
        // bypassed the WAL, so an acked SET could be lost on a crash before the
        // next flush — a false ack under Durable mode. Route through the batch
        // commit path like PUT/DELETE.
        //
        // Re-gravitation: if this SET changed the placement (gravity) field, the
        // record's gravity_hash no longer matches its SpatialKey — it would be
        // stranded in the old bucket, invisible to a SCAN by its new gravity
        // value. Recompute the canonical hash (the SAME keel the SCAN fast path
        // resolves) and, when it differs, atomically MOVE the record: remove the
        // old SpatialKey, write the new one (fresh seq → no collision), and
        // repoint identity. One batch ⇒ crash-atomic.
        let old_gh = SpatialKey::gravity_hash_from_bytes(&sk_array);
        let new_gh = crate::ops::put::gravity_hash_for(engine, &record.lobe_name, &rec.fields);
        // Sub-gravity: a SET that changes the satellite field must MOVE the
        // record to its new satellite, exactly as a changed gravity field
        // re-buckets it — else the record is stranded in the old satellite,
        // invisible to a bounded (per-satellite) query on its new value.
        // old_sat is the key's bytes 8..10; new_sat comes from the post-SET
        // fields (None ⇒ the lobe has no satellite axis, sat stays 0).
        let old_sat = u16::from_be_bytes([sk_array[8], sk_array[9]]);
        let new_sat = engine.satellite_sat_for(&record.lobe_name, &rec.fields);
        let sat_changed = new_sat.is_some_and(|ns| ns != old_sat);
        let mut batch = engine.turba.batch();
        let final_sk: [u8; SPATIAL_KEY_SIZE] = if new_gh != old_gh || sat_changed {
            let type_id = crate::ops::put::type_id_from_fields(&rec.fields);
            let ts = normalize_timestamp(now as u64);
            let seq = crate::ops::put::next_record_seq();
            let new_sk = match new_sat {
                Some(ns) => SpatialKey::new_with_sat(lobe_id, new_gh, ns, type_id, ts, seq),
                None => SpatialKey::new(lobe_id, new_gh, type_id, ts, seq),
            }
            .to_bytes();
            batch.remove_spatial(&sk_array);
            batch.put_spatial(new_sk.as_slice(), updated.as_slice());
            batch.put_identity(&lid_bytes, new_sk.as_slice());
            // Move the column with the record: drop the old-key entry, write the
            // new one. A non-vector record has no column on either key.
            batch.remove_vectors(&sk_array);
            if let Some(col) = &vec_column {
                batch.put_vectors(new_sk.as_slice(), col.as_slice());
            }
            new_sk
        } else {
            batch.put_spatial(&sk_array, updated.as_slice());
            // Rewrite the column in place under the unchanged spatial key.
            if let Some(col) = &vec_column {
                batch.put_vectors(&sk_array, col.as_slice());
            }
            sk_array
        };
        if let Some((k, v)) = &vec_reg_entry {
            batch.put_dictionary(k.as_slice(), v.as_slice());
        }
        batch
            .commit()
            .map_err(|e| XyzError::Storage(format!("set commit failed: {e}")))?;

        // Ghost V2 post-write hook (uses the final placement key, which differs
        // from sk_array when the record was re-gravitated).
        engine.ghost_manager.notify_write(
            lobe_id,
            &rec,
            final_sk.as_slice(),
            crate::ghost::WriteType::Update {
                old_record,
                // Pre-SET spatial key: differs from `final_sk` when the SET
                // re-gravitated the record, so ghost maintenance can find and
                // move the old entry instead of dangling it.
                old_spatial_key: sk_array.to_vec(),
            },
        );

        // V5: Write-through to RecordCache
        if let Some(cache) = &engine.record_cache {
            cache.update_record(lobe_id, &rec);
        }
        count += 1;
    }

    Ok(QueryResult::Ok {
        lid: records.first().map(|r| r.lid),
        message: format!("{count} record(s) updated"),
    })
}
