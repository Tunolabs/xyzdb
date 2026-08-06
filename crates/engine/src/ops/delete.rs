// SPDX-License-Identifier: BUSL-1.1
use crate::anchor::dictionary_key;
use crate::engine::{Engine, QueryResult};
use xytalk_parser::ast::{DeleteStmt, FindTarget, PurgeStmt};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::SPATIAL_KEY_SIZE;
use xyzdb_core::record::Record;
use xyzdb_core::value::Value;

/// Execute DELETE: remove records (tombstone in Turba).
pub fn execute_delete(
    engine: &Engine,
    stmt: DeleteStmt,
    source_records: Option<Vec<Record>>,
) -> Result<QueryResult> {
    let records = match source_records {
        Some(recs) => recs,
        None => {
            let target = stmt.target.as_ref().ok_or_else(|| {
                XyzError::InvalidQuery("DELETE requires a target or pipeline".into())
            })?;
            // Standalone DELETE honours an optional WHERE (xyTalk v1 P1: full
            // OR/NOT/IN tree). No WHERE still means "all under target" (P7
            // require-WHERE is a later wave). AND-pure takes the fast path;
            // OR/NOT scans the target + walker-filters.
            let found = engine.resolve_find_expr(target, &stmt.filter_expr)?;
            found.into_iter().map(|(r, _)| r).collect()
        }
    };

    if records.is_empty() {
        return Ok(QueryResult::Ok {
            lid: None,
            message: "0 records deleted".into(),
        });
    }

    let mut count = 0u64;
    for record in &records {
        delete_single_record(engine, record)?;
        count += 1;
    }

    Ok(QueryResult::Ok {
        lid: records.first().map(|r| r.lid),
        message: format!("{count} record(s) deleted"),
    })
}

/// Execute PURGE: empty a whole lobe. Total deletion routed through the exact
/// same per-record delete path as a WHERE-matching DELETE, so ghosts and indexes
/// are maintained (each removal fires `notify_write`). It is the explicit,
/// hard-to-typo spelling — `DELETE` now refuses to run without a WHERE.
pub fn execute_purge(engine: &Engine, stmt: PurgeStmt) -> Result<QueryResult> {
    let delete_all = DeleteStmt {
        target: Some(FindTarget::Lobe(stmt.lobe)),
        filter_expr: None,
    };
    execute_delete(engine, delete_all, None)
}

fn delete_single_record(engine: &Engine, record: &Record) -> Result<()> {
    let lid_bytes = record.lid.to_bytes();

    // Get spatial key from identity
    let sk_val = match engine
        .turba
        .identity
        .get(&lid_bytes)
        .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
    {
        Some(v) => v,
        None => return Ok(()), // already deleted
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

    // Remove INDEX-BEFORE-DATA. The batch's items are applied to the
    // per-keyspace memtables one at a time (engine commit loop), each
    // independently visible to concurrent readers, and the keyspaces are not
    // cross-atomic. So the spatial record (the data) must be tombstoned
    // LAST: every pointer into it — the anchor dictionary entries and the
    // identity (LID → spatial) mapping — is removed first, so a racing
    // reader can never resolve a live pointer to a tombstoned record (a
    // dangling index). This mirrors PUT, which writes the data before the
    // index. `build_delete_ops` owns the ordering; keep it the single source.
    let ops = build_delete_ops(engine, record, lid_bytes.as_slice(), &sk_array);
    let mut batch = engine.turba.batch();
    for op in &ops {
        match op {
            DeleteOp::Dictionary(key) => batch.remove_dictionary(key),
            DeleteOp::Identity(key) => batch.remove_identity(key),
            DeleteOp::Vectors(key) => batch.remove_vectors(key.as_slice()),
            DeleteOp::Spatial(key) => batch.remove_spatial(key.as_slice()),
        }
    }

    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("delete batch commit: {e}")))?;

    // Ghost V2 post-write hook
    {
        let lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
        engine.ghost_manager.notify_write(
            lobe_id,
            record,
            sk_array.as_slice(),
            crate::ghost::WriteType::Delete,
        );
    }

    // V5: Invalidate from RecordCache
    if let Some(cache) = &engine.record_cache {
        let lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
        cache.invalidate_record(lobe_id, &record.lid);
    }

    Ok(())
}

/// One keyspace removal in a single-record DELETE. The variant order in a
/// built sequence encodes the durability/visibility contract: every pointer
/// (`Dictionary`, `Identity`) and the derived `Vectors` column are removed
/// before the `Spatial` data they reference.
enum DeleteOp {
    Dictionary(Vec<u8>),
    Identity(Vec<u8>),
    Vectors([u8; SPATIAL_KEY_SIZE]),
    Spatial([u8; SPATIAL_KEY_SIZE]),
}

/// Build a record's removal sequence in INDEX-BEFORE-DATA order: anchor
/// dictionary entries, then the identity (LID → spatial) pointer, then the
/// spatial record itself LAST. This is the single source of truth for the
/// ordering (see `delete_single_record` for why it matters); the
/// `delete_removes_index_before_data` test pins it.
fn build_delete_ops(
    engine: &Engine,
    record: &Record,
    lid_bytes: &[u8],
    sk_array: &[u8; SPATIAL_KEY_SIZE],
) -> Vec<DeleteOp> {
    let mut ops: Vec<DeleteOp> = Vec::new();

    // 1. Anchor dictionary entries (the secondary index into this record).
    let lobes = engine.lobe_registry.read();
    if let Some(lobe_config) = lobes.get(&record.lobe_name) {
        let lobe_id = lobe_config.id;
        let anchors = engine.anchor_registry.read();
        for anchor_name in anchors.get_anchors(&record.lobe_name) {
            if let Some(val) = record.fields.get(anchor_name) {
                let val_str = match val {
                    Value::Text(s) => s.clone(),
                    other => format!("{other}"),
                };
                ops.push(DeleteOp::Dictionary(dictionary_key(
                    lobe_id,
                    anchor_name,
                    &val_str,
                )));
            }
        }
    }
    drop(lobes);

    // 2. The identity pointer (LID → spatial).
    ops.push(DeleteOp::Identity(lid_bytes.to_vec()));

    // 3. The V5 vector column (keyed by the same spatial key). It is DERIVED
    // from the record, so it is removed with the index — before the data. A
    // record with no column has an inert tombstone here (no live entry to find).
    ops.push(DeleteOp::Vectors(*sk_array));

    // 4. The data itself — LAST, so no pointer outlives it.
    ops.push(DeleteOp::Spatial(*sk_array));

    ops
}

#[cfg(test)]
mod delete_order_tests {
    use super::*;
    use crate::engine::Engine;

    fn exec(engine: &Engine, s: &str) -> QueryResult {
        let stmt = xytalk_parser::parse(s).unwrap_or_else(|e| panic!("parse {s:?}: {e:?}"));
        engine
            .execute(stmt)
            .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
    }

    /// The DELETE batch must tombstone the data (spatial) only AFTER every
    /// pointer into it, so a concurrent reader never resolves a live anchor
    /// or identity entry to a tombstoned record. Domain-neutral vocab.
    #[test]
    fn delete_removes_index_before_data() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        exec(&engine, r#"LOBE "items""#);
        exec(&engine, r#"ANCHOR "key" UNIQUE IN "items""#);
        exec(&engine, r#"PUT {key: "K1", data: "v"} IN "items""#);

        let rec = match exec(&engine, r#"SCAN "items""#) {
            QueryResult::Records(mut r) => r.pop().expect("one record stored"),
            other => panic!("unexpected scan result: {other:?}"),
        };
        let lid_bytes = rec.lid.to_bytes();
        // Content is irrelevant to the ordering under test.
        let sk = [0u8; SPATIAL_KEY_SIZE];

        let ops = build_delete_ops(&engine, &rec, lid_bytes.as_slice(), &sk);

        let spatial_idx = ops
            .iter()
            .position(|o| matches!(o, DeleteOp::Spatial(_)))
            .expect("a data removal");
        // Data is removed last.
        assert_eq!(
            spatial_idx,
            ops.len() - 1,
            "the data (spatial) removal must be the final op"
        );
        // The anchored record removes its dictionary index, and every
        // pointer (dictionary + identity) precedes the data removal.
        let dict_count = ops
            .iter()
            .filter(|o| matches!(o, DeleteOp::Dictionary(_)))
            .count();
        assert!(
            dict_count >= 1,
            "an anchored record must remove its anchor index"
        );
        for (i, o) in ops.iter().enumerate() {
            match o {
                DeleteOp::Dictionary(_) | DeleteOp::Identity(_) | DeleteOp::Vectors(_) => assert!(
                    i < spatial_idx,
                    "pointer/column removal at {i} must precede the data removal at {spatial_idx}"
                ),
                DeleteOp::Spatial(_) => {}
            }
        }
    }
}
