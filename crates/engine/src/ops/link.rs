// SPDX-License-Identifier: BUSL-1.1
use crate::engine::{Engine, QueryResult};
use xytalk_parser::ast::{LinkClause, LinkStmt};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::SPATIAL_KEY_SIZE;
use xyzdb_core::lid::LID;
use xyzdb_core::value::Value;

/// Execute standalone LINK: create an edge between two existing records.
pub fn execute_link(engine: &Engine, stmt: LinkStmt) -> Result<QueryResult> {
    // Standalone LINK honours optional WHERE on both sides (xyTalk v1 P1: full
    // OR/NOT/IN tree). No WHERE keeps the "first record under target" semantics;
    // AND-pure takes the fast path, OR/NOT scans + walker-filters.
    let source_records = engine.resolve_find_expr(&stmt.source, &stmt.source_filter_expr)?;
    let source = source_records
        .first()
        .ok_or_else(|| XyzError::RecordNotFound("LINK source not found".into()))?;

    let target_records = engine.resolve_find_expr(&stmt.target, &stmt.target_filter_expr)?;
    let target = target_records
        .first()
        .ok_or_else(|| XyzError::RecordNotFound("LINK target not found".into()))?;

    // Store link as metadata fields on the source record
    let source_lid = source.0.lid;
    let target_lid = target.0.lid;

    // Update source record with link info
    let lid_bytes = source_lid.to_bytes();
    let sk_val = engine
        .turba
        .identity
        .get(&lid_bytes)
        .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
        .ok_or_else(|| XyzError::RecordNotFound(source_lid.to_string()))?;

    let sk_array: [u8; SPATIAL_KEY_SIZE] = if sk_val.len() == SPATIAL_KEY_SIZE {
        sk_val
            .as_slice()
            .try_into()
            .map_err(|_| XyzError::Internal("bad spatial key".into()))?
    } else if sk_val.len() == 10 {
        let mut arr = [0u8; SPATIAL_KEY_SIZE];
        arr[..10].copy_from_slice(&sk_val);
        arr
    } else {
        return Err(XyzError::Internal("bad spatial key".into()));
    };

    let rec_val = engine
        .turba
        .spatial
        .get(&sk_array)
        .map_err(|e| XyzError::Storage(format!("spatial get: {e}")))?
        .ok_or_else(|| XyzError::RecordNotFound(source_lid.to_string()))?;

    let link_lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
    let link_lobe_name = engine.lobe_name_for_id(link_lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(link_lobe_id);
    let mut record = xyzdb_core::record::deserialize_record(&rec_val, &link_lobe_name, fd)?;
    drop(fr_guard);

    // Add link as _link_<name> field
    let link_field = format!("_link_{}", stmt.relation_name);
    record
        .fields
        .insert(link_field, Value::Text(target_lid.to_string()));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    record.updated_at = now;

    // WAL-durable update (V1 is self-contained — no registry entry needed).
    let updated = xyzdb_core::record::serialize_record(&record);
    let mut batch = engine.turba.batch();
    batch.put_spatial(&sk_array, updated.as_slice());
    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("link commit failed: {e}")))?;

    Ok(QueryResult::Ok {
        lid: Some(source_lid),
        message: format!(
            "LINK created: {} -> {} as '{}'",
            source_lid, target_lid, stmt.relation_name
        ),
    })
}

/// Create a link from a newly inserted record to a target (called from PUT with LINK clause).
pub fn create_link_record(engine: &Engine, source_lid: LID, link: &LinkClause) -> Result<()> {
    let target_records = engine.resolve_find(&link.target, &link.filters)?;
    let target = match target_records.first() {
        Some(t) => t,
        None => return Ok(()), // target not found, skip link silently
    };

    let target_lid = target.0.lid;

    // Read source record and add link field
    let lid_bytes = source_lid.to_bytes();
    let sk_val = engine
        .turba
        .identity
        .get(&lid_bytes)
        .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
        .ok_or_else(|| XyzError::RecordNotFound(source_lid.to_string()))?;

    let sk_array: [u8; SPATIAL_KEY_SIZE] = if sk_val.len() == SPATIAL_KEY_SIZE {
        sk_val
            .as_slice()
            .try_into()
            .map_err(|_| XyzError::Internal("bad spatial key".into()))?
    } else if sk_val.len() == 10 {
        let mut arr = [0u8; SPATIAL_KEY_SIZE];
        arr[..10].copy_from_slice(&sk_val);
        arr
    } else {
        return Err(XyzError::Internal("bad spatial key".into()));
    };

    let rec_val = engine
        .turba
        .spatial
        .get(&sk_array)
        .map_err(|e| XyzError::Storage(format!("spatial get: {e}")))?
        .ok_or_else(|| XyzError::RecordNotFound(source_lid.to_string()))?;

    let cl_lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
    let cl_lobe_name = engine.lobe_name_for_id(cl_lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(cl_lobe_id);
    let mut record = xyzdb_core::record::deserialize_record(&rec_val, &cl_lobe_name, fd)?;
    drop(fr_guard);

    let link_field = format!("_link_{}", link.relation_name);
    record
        .fields
        .insert(link_field, Value::Text(target_lid.to_string()));

    // Serialize V2 and co-commit any new id->name mapping in the SAME batch as
    // the record: the mapping is durable iff the record is, so an acked edge is
    // always decodable after a crash. Tree::insert (the old path) wrote the
    // active memtable only and bypassed the WAL — an acked edge could vanish on
    // a crash before the next flush.
    let (updated, reg_entry) = {
        let mut fr = engine.field_registry.write();
        fr.serialize_record_v2_durable(cl_lobe_id, &record)?
    };
    let mut batch = engine.turba.batch();
    batch.put_spatial(&sk_array, updated.as_slice());
    if let Some((key, val)) = &reg_entry {
        batch.put_dictionary(key.as_slice(), val.as_slice());
    }
    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("link commit failed: {e}")))?;

    Ok(())
}
