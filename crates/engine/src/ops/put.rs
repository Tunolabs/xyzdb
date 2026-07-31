use crate::anchor::dictionary_key;
use crate::engine::{Engine, QueryResult};
use crate::gravity_spec::GravitySpec;
use crate::ops::literal_to_value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use xytalk_parser::ast::{FindTarget, PutBatchStmt, PutStmt};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::{SPATIAL_KEY_SIZE, SpatialKey, hash_to_48bits, normalize_timestamp};
use xyzdb_core::lid::LID;
use xyzdb_core::record::{Record, serialize_record};
use xyzdb_core::value::Value;

/// Global monotonic counter for SpatialKey uniqueness.
/// At 1M inserts/s, overflow in ~584,942 years.
static RECORD_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn execute_put(engine: &Engine, stmt: PutStmt) -> Result<QueryResult> {
    // 1. Resolve lobe (auto-create if needed)
    let lobe_id = {
        let mut lobes = engine.lobe_registry.write();
        let id = lobes.get_or_create(&stmt.lobe, None);
        engine.persist_lobe_registry(&lobes)?;
        id
    };

    // 2. Build fields map, collect gravity fields, inject _type if missing
    let mut fields = BTreeMap::new();
    let mut has_type = false;
    let mut gravity_fields: Vec<(String, Value)> = Vec::new();
    for pf in &stmt.fields {
        if pf.name == "_type" {
            has_type = true;
        }
        let val = literal_to_value(&pf.value);
        if pf.gravity {
            gravity_fields.push((pf.name.clone(), val.clone()));
        }
        fields.insert(pf.name.clone(), val);
    }
    if !has_type {
        fields.insert("_type".into(), Value::Text(stmt.lobe.clone()));
    }

    // 3d-4: a record marking 2+ gravity fields must have a declared composite
    // spec; otherwise placement silently collapses to the first marker. Check
    // before auto-registering so an erroring PUT leaves no spurious Raw spec.
    if gravity_fields.len() >= 2
        && !matches!(
            engine.get_gravity_spec(&stmt.lobe),
            Some(GravitySpec::Composite(_))
        )
    {
        return Err(multi_gravity_marker_error(&stmt.lobe, &gravity_fields));
    }

    // Finding 13: register the lobe's gravity field on the first PUT carrying a
    // single `*` marker. Idempotent fast path for subsequent PUTs (read-lock
    // check in Engine::register_gravity_field). Multi-`*` either errored above
    // or has a declared composite spec, so it does not auto-register a Raw.
    if let [(gf_name, _)] = gravity_fields.as_slice() {
        engine.register_gravity_field(&stmt.lobe, gf_name)?;
    }

    // 3. Check anchors — verify uniqueness or handle ON CONFLICT UPDATE.
    // 2b: collect the anchor keys (brief registry read, then released), then
    // serialize the uniqueness check + the batch commit below on the per-anchor
    // shard so concurrent same-anchor PUTs cannot both pass the check (TOCTOU).
    // Lock order: registry -> shard -> memtable (see 8d); the shard is acquired
    // AFTER the registry read is dropped and held until end of scope (commit).
    let anchor_keys: Vec<(String, String, Vec<u8>)> = {
        let anchors = engine.anchor_registry.read();
        anchors
            .get_anchors(&stmt.lobe)
            .iter()
            .filter_map(|name| {
                fields.get(name).map(|v| {
                    let val_str = value_to_anchor_string(v);
                    let dict_key = dictionary_key(lobe_id, name, &val_str);
                    (name.clone(), val_str, dict_key)
                })
            })
            .collect()
    };
    let dict_keys: Vec<Vec<u8>> = anchor_keys.iter().map(|(_, _, dk)| dk.clone()).collect();
    let _anchor_guards = engine.lock_anchor_shards(&dict_keys);

    for (anchor_name, val_str, dict_key) in &anchor_keys {
        if let Some(existing) = engine
            .turba
            .dictionary
            .get(dict_key)
            .map_err(|e| XyzError::Storage(format!("dictionary get: {e}")))?
        {
            // Anchor collision
            if stmt.on_conflict.is_some() {
                // ON CONFLICT UPDATE — update existing record
                let existing_lid = LID::from_bytes(
                    &<[u8; 16]>::try_from(existing.as_slice())
                        .map_err(|_| XyzError::Internal("bad LID in dictionary".into()))?,
                );
                return execute_upsert(engine, existing_lid, &fields, &stmt.lobe);
            }

            let existing_lid_str = if existing.len() == 16 {
                let lid = LID::from_bytes(
                    &<[u8; 16]>::try_from(existing.as_slice())
                        .map_err(|_| XyzError::Internal("bad LID".into()))?,
                );
                lid.to_string()
            } else {
                "unknown".into()
            };

            return Err(XyzError::DuplicateAnchor {
                anchor: anchor_name.clone(),
                value: val_str.clone(),
                lobe: stmt.lobe.clone(),
                existing_lid: existing_lid_str,
            });
        }
    }

    // 4. Determine gravity_hash
    // Priority: LINK TO (explicit) > *gravity (implicit) > anchor > fallback
    let gravity_hash = if let Some(ref link_clause) = stmt.link {
        resolve_gravity_hash_from_link(engine, &link_clause.target, &link_clause.filters)?
    } else if !gravity_fields.is_empty() {
        // Route through the lobe's registered spec (set to Raw(first `*`) just
        // above) so placement and the SCAN fast path resolve the same hash —
        // the keel. For a single `*` this is byte-identical to the old
        // compute_gravity_hash; for multiple `*` it folds only the registered
        // field, matching the query side instead of diverging (the footgun).
        // The fallback covers the unexpected no-spec case.
        let spec = engine.get_gravity_spec(&stmt.lobe);
        let keel = spec.as_ref().and_then(|s| s.compute_hash(&fields));
        if let Some(ref s) = spec {
            // #11: co-located on the declared keel. Count for the
            // gravity-declared denominator (health signal, placement unchanged).
            engine.observe_keel(&stmt.lobe, s, keel.is_some());
        }
        keel.unwrap_or_else(|| compute_gravity_hash(&gravity_fields))
    } else {
        // No `*` marker: still honour a declared GravitySpec (`GRAVITY BY …`) so a
        // record whose gravity field is written as a *plain* field lands in the same
        // bucket the SCAN/FIND fast path resolves. A gravity field is also a normal
        // field; declaring the keel must not require the `*` sugar at write time, or
        // placement (write) and `detect_gravity_eq` (query) diverge and the bucket
        // scan finds nothing. Falls back to anchor/LID gravity only when no spec pins
        // this record (no spec, or the spec's field absent from the record).
        let spec = engine.get_gravity_spec(&stmt.lobe);
        let keel = spec.as_ref().and_then(|s| s.compute_hash(&fields));
        if let Some(ref s) = spec {
            // #11: keel present → co-located; absent (case C) → anchor/LID
            // fallback bucket = silent scoped-recall loss. Count either way so a
            // gravity-declared lobe surfaces the omit ratio instead of degrading
            // quietly. Placement itself is unchanged.
            engine.observe_keel(&stmt.lobe, s, keel.is_some());
        }
        keel.unwrap_or_else(|| compute_record_gravity_hash(engine, &stmt.lobe, &fields))
    };

    // 5. Generate LID
    let lid = LID::new(lobe_id);

    // 6. Build SpatialKey
    let now_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let type_id = type_id_from_fields(&fields);
    let seq = RECORD_SEQ.fetch_add(1, Ordering::Relaxed);
    let spatial_key = SpatialKey::new(
        lobe_id,
        gravity_hash,
        type_id,
        normalize_timestamp(now_micros),
        seq,
    );
    let spatial_bytes = spatial_key.to_bytes();

    // 7. Build Record
    let record = Record {
        lid,
        lobe_name: stmt.lobe.clone(),
        fields,
        created_at: now_micros as i64,
        updated_at: now_micros as i64,
    };
    // V5 split: a lobe with a declared searchable vector whose record actually
    // carries that Vector serializes V5 (blob WITHOUT the vector) plus a separate
    // column entry written under the SAME spatial key in the `vectors` keyspace.
    // Hoisting the vector out of the blob is the RAM win — NEAREST scans only the
    // ~1 KB column, not the ~4 KB blob. Every other record — no spec, or spec but
    // field absent/not a Vector — stays on the existing V1 `serialize_record`
    // path, byte-identical to before.
    let search = engine.get_vector_spec(&stmt.lobe).map(|s| s.field);
    // Validate (and learn on the first write) the searchable vector's dimension
    // before hoisting it, so a wrong-model / malformed embedding is rejected at
    // ingest instead of being silently unsearchable at query time.
    if let Some(f) = &search
        && let Some(Value::Vector(v)) = record.fields.get(f)
    {
        engine.ensure_vector_dim(&stmt.lobe, f, v.len())?;
    }
    let (record_bytes, vec_column, vec_reg_entry) = match &search {
        Some(f) if matches!(record.fields.get(f), Some(Value::Vector(_))) => engine
            .field_registry
            .write()
            .serialize_record_v5_durable(lobe_id, &record, Some(f))?,
        _ => (serialize_record(&record), None, None),
    };

    // 8. Atomic batch write: spatial + identity + dictionary anchors
    let mut batch = engine.turba.batch();
    batch.put_spatial(spatial_bytes.as_slice(), record_bytes.as_slice());
    batch.put_identity(lid.to_bytes().as_slice(), spatial_bytes.as_slice());
    // The V5 vector column, keyed by the SAME spatial key as the blob, so the
    // bucket range scan over `vectors` matches `spatial` byte-for-byte.
    if let Some(col) = &vec_column {
        batch.put_vectors(spatial_bytes.as_slice(), col.as_slice());
    }
    // Co-commit the V3 field-id→name mapping (when the dict grew) in the SAME
    // batch as the record, like link.rs — durable iff the record is.
    if let Some((k, v)) = &vec_reg_entry {
        batch.put_dictionary(k.as_slice(), v.as_slice());
    }

    // Write anchor entries to the dictionary. Reuse the `anchor_keys` snapshot
    // taken (and released) before the shard locks — re-reading anchor_registry
    // here would acquire it UNDER the held shard locks, inverting the documented
    // registry -> shard -> memtable order (8d). The snapshot already holds the
    // (name, value, dict_key) for every anchor present on this record.
    for (_, _, dict_key) in &anchor_keys {
        batch.put_dictionary(dict_key.as_slice(), lid.to_bytes().as_slice());
    }

    // No per-value gravity dictionary entry is written: gravity values are
    // 1→N (a bucket, not an identity), so a single-LID entry can only
    // misrepresent the bucket — FIND resolves gravity predicates via the
    // bounded bucket range scan instead (0.7.5; pre-0.7.5 entries also
    // leaked on DELETE because N-membership makes them un-refcountable).

    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("batch commit failed: {e}")))?;

    // 9. Ghost post-write hook
    engine.ghost_manager.notify_write(
        lobe_id,
        &record,
        spatial_bytes.as_slice(),
        crate::ghost::WriteType::Insert,
    );

    // 10. If LINK clause, create the link edge
    if let Some(link_clause) = stmt.link {
        crate::ops::link::create_link_record(engine, lid, &link_clause)?;
    }

    // 11. Record write in ghost router (for staleness tracking)
    if let Some(router) = engine.ghost_routers.read().get(&lobe_id) {
        router.record_writes(1);
    }

    // V5: Write-through to RecordCache if lobe is cached
    if let Some(cache) = &engine.record_cache {
        cache.update_record(lobe_id, &record);
    }

    Ok(QueryResult::Ok {
        lid: Some(lid),
        message: format!("1 record inserted (LID: {})", lid),
    })
}

/// Compute type_id from the _type field for SpatialKey differentiation.
/// Records with same gravity (gravity_hash) but different _type get different
/// SpatialKeys → co-located on disk but never overwrite each other.
pub(crate) fn type_id_from_fields(fields: &BTreeMap<String, Value>) -> u16 {
    match fields.get("_type") {
        Some(Value::Text(t)) => (hash_to_48bits(t) & 0xFFFF) as u16,
        _ => 0,
    }
}

/// The placement `gravity_hash` for `fields` of `lobe`, via the lobe's
/// registered `GravitySpec` (the keel — the SAME hash the SCAN fast path
/// resolves), falling back to the anchor/LID hash when no spec is set. Used by
/// re-gravitation (a SET that changes the gravity field moves the record).
pub(crate) fn gravity_hash_for(
    engine: &Engine,
    lobe: &str,
    fields: &BTreeMap<String, Value>,
) -> u64 {
    engine
        .get_gravity_spec(lobe)
        .and_then(|spec| spec.compute_hash(fields))
        .unwrap_or_else(|| compute_record_gravity_hash(engine, lobe, fields))
}

/// A fresh, globally-unique record seq — for re-placing a moved record into a
/// new bucket without colliding with existing entries.
pub(crate) fn next_record_seq() -> u64 {
    RECORD_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Resolve gravity_hash from a LINK target's existing SpatialKey.
fn resolve_gravity_hash_from_link(
    engine: &Engine,
    target: &FindTarget,
    filters: &[xytalk_parser::ast::Filter],
) -> Result<u64> {
    let results = engine.resolve_find(target, filters)?;
    let (_record, spatial_bytes) = results
        .first()
        .ok_or_else(|| XyzError::RecordNotFound("LINK target not found".into()))?;

    Ok(SpatialKey::gravity_hash_from_bytes(spatial_bytes))
}

/// Compute the canonical gravity_hash from gravity field values.
///
/// D1: the hash folds **values only** (the field name is NOT part of the input),
/// joined by `\0` for a composite, each canonicalized via [`value_to_anchor_string`].
/// Value-only is the single canonical convention: it matches the anchor/LID
/// fallback ([`compute_record_gravity_hash`]), `LINK` (which inherits the
/// target's bucket), and `PLACE GROUP` (place.rs) — so a record reaches the same
/// bucket regardless of how it was placed, and a `WHERE field = X` scan probes
/// that bucket. The pre-0.8 `name\0value` form put `*`-placed rows in a different
/// bucket than anchor/LINK-placed rows for the same value (the gravity-as-index
/// miss; see `tests/gravity_index_anchor_asymmetry.rs`).
///
/// Exposed as `pub(crate)` so the SCAN equality fast path can compute the same
/// hash from a `WHERE gravity_field = X` predicate to bound its range scan.
pub(crate) fn compute_gravity_hash(gravity_fields: &[(String, Value)]) -> u64 {
    let mut combined = String::new();
    for (_name, val) in gravity_fields {
        if !combined.is_empty() {
            combined.push('\0');
        }
        combined.push_str(&value_to_anchor_string(val));
    }
    hash_to_48bits(&combined)
}

/// Error for a record marking 2+ gravity fields without a declared composite
/// spec.
///
/// Such a record's placement would silently collapse to the first marker (the
/// keel folds only the registered field), diverging from the user's intent.
/// The fix is explicit: declare the composite once with `GRAVITY BY (a, b, …)`.
fn multi_gravity_marker_error(lobe: &str, gravity_fields: &[(String, Value)]) -> XyzError {
    let list = gravity_fields
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    XyzError::InvalidQuery(format!(
        "record marks multiple gravity fields ({list}) in lobe '{lobe}' but no composite gravity \
         spec is declared; declare it with `GRAVITY BY ({list}) IN \"{lobe}\"` before writing — \
         multiple `*` markers without a declaration would co-locate by only the first field"
    ))
}

/// Compute gravity_hash from the first anchor value present, or — when the
/// record has no anchor field — from a hash of the record's `{:?}` fields
/// (not the LID).
///
/// Value-only (no field name), so it shares the canonical convention with
/// [`compute_gravity_hash`]. Exposed `pub(crate)` so the D1 rehash migration can
/// reproduce a record's placement when the lobe has no `*` spec for it.
pub(crate) fn compute_record_gravity_hash(
    engine: &Engine,
    lobe: &str,
    fields: &BTreeMap<String, Value>,
) -> u64 {
    let anchors = engine.anchor_registry.read();
    let anchor_fields = anchors.get_anchors(lobe);

    // Use first anchor value found in fields
    for anchor_name in anchor_fields {
        if let Some(val) = fields.get(anchor_name) {
            let s = value_to_anchor_string(val);
            return hash_to_48bits(&s);
        }
    }

    // No anchor → hash from a unique-ish combo
    let fallback = format!("{:?}", fields);
    hash_to_48bits(&fallback)
}

/// ON CONFLICT UPDATE: modify the existing record's fields.
fn execute_upsert(
    engine: &Engine,
    existing_lid: LID,
    new_fields: &BTreeMap<String, Value>,
    lobe_name: &str,
) -> Result<QueryResult> {
    // Read existing record via identity → spatial
    let lid_bytes = existing_lid.to_bytes();
    let spatial_key_bytes = engine
        .turba
        .identity
        .get(&lid_bytes)
        .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
        .ok_or_else(|| XyzError::RecordNotFound(existing_lid.to_string()))?;

    let sk_array: [u8; SPATIAL_KEY_SIZE] = if spatial_key_bytes.len() == SPATIAL_KEY_SIZE {
        spatial_key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| XyzError::Internal("bad spatial key in identity".into()))?
    } else if spatial_key_bytes.len() == 10 {
        // Legacy 10-byte key: pad with seq=0
        let mut arr = [0u8; SPATIAL_KEY_SIZE];
        arr[..10].copy_from_slice(&spatial_key_bytes);
        arr
    } else {
        return Err(XyzError::Internal("bad spatial key in identity".into()));
    };

    let record_bytes = engine
        .turba
        .spatial
        .get(&sk_array)
        .map_err(|e| XyzError::Storage(format!("spatial get: {e}")))?
        .ok_or_else(|| XyzError::RecordNotFound(existing_lid.to_string()))?;

    let lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
    let fd_ref = engine.field_registry.read();
    let fd = fd_ref.get_dict(lobe_id);
    // Hydrate the existing search vector from the `vectors` column: a V5 blob
    // decodes WITHOUT it, and an update that does not re-supply the vector field
    // must not silently drop it on rewrite.
    let mut record =
        crate::ops::deserialize_hydrated(engine, &sk_array, &record_bytes, lobe_name, fd)?;
    drop(fd_ref);

    // P10: snapshot the pre-merge record so the ghost update can compute the
    // aggregate delta (subtract old, add new). Cheap clone, once per upsert.
    let old_record = record.clone();

    // Merge new fields into existing
    for (k, v) in new_fields {
        record.fields.insert(k.clone(), v.clone());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;
    record.updated_at = now;

    // WAL-durable update. Tree::insert wrote the active memtable only and
    // bypassed the WAL, so an acked PUT-update could be lost on a crash before
    // the next flush — a false ack under Durable mode. Route through the batch
    // commit path like the insert branch.
    // Same V5-split decision as the insert path: hoist the vector to the column
    // only when the lobe has a declared vector AND the (post-merge) record
    // carries it; otherwise V1. The column is rewritten under the unchanged
    // spatial key (in-place update — no re-bucket here).
    let search = engine.get_vector_spec(lobe_name).map(|s| s.field);
    // Same dimension guard as the insert path: an ON CONFLICT UPDATE that
    // re-supplies the vector must match the field's learned dimension.
    if let Some(f) = &search
        && let Some(Value::Vector(v)) = record.fields.get(f)
    {
        engine.ensure_vector_dim(lobe_name, f, v.len())?;
    }
    let (updated_bytes, vec_column, vec_reg_entry) = match &search {
        Some(f) if matches!(record.fields.get(f), Some(Value::Vector(_))) => engine
            .field_registry
            .write()
            .serialize_record_v5_durable(lobe_id, &record, Some(f))?,
        _ => (serialize_record(&record), None, None),
    };
    let mut batch = engine.turba.batch();
    batch.put_spatial(&sk_array, updated_bytes.as_slice());
    if let Some(col) = &vec_column {
        batch.put_vectors(&sk_array, col.as_slice());
    }
    if let Some((k, v)) = &vec_reg_entry {
        batch.put_dictionary(k.as_slice(), v.as_slice());
    }
    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("put update commit failed: {e}")))?;

    // P10 (was TODO(ghost-upsert-notify)): notify ghosts of the in-place update
    // so covering ghosts refresh the record and aggregate ghosts adjust their
    // sums/counts via the old_record → record delta — previously they kept
    // stale values until REFRESH. This path writes in place (no re-bucket), so
    // the old and new spatial keys are the same `sk_array` (unlike SET, which
    // may re-gravitate).
    engine.ghost_manager.notify_write(
        lobe_id,
        &record,
        &sk_array,
        crate::ghost::WriteType::Update {
            old_record,
            old_spatial_key: sk_array.to_vec(),
        },
    );

    // Sibling of P10 (same "upsert doesn't propagate" class): the upsert also
    // skipped the RecordCache write-through that insert (above) and SET both do,
    // so a cached read returned the pre-upsert record. Mirror them.
    if let Some(cache) = &engine.record_cache {
        cache.update_record(lobe_id, &record);
    }

    Ok(QueryResult::Ok {
        lid: Some(existing_lid),
        message: format!("1 record updated (LID: {})", existing_lid),
    })
}

/// Canonical string form of a value for anchor keys and gravity hashing.
/// PULL's collision post-filter compares through this same canon so its
/// equality classes match the write-side `compute_gravity_hash` input.
pub(crate) fn value_to_anchor_string(val: &Value) -> String {
    match val {
        Value::Text(s) => s.clone(),
        Value::Int(v) => v.to_string(),
        Value::Float(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
        other => format!("{other}"),
    }
}

// ─── PUT BATCH ────────────────────────────────────────────────────────────────

const MAX_BATCH_SIZE: usize = 10_000;

/// Execute PUT BATCH: insert multiple records as a single Turba write batch
/// (one atomic WAL commit). Visibility across keyspaces is eventually
/// consistent, NOT a read-atomic snapshot — order within the batch matters
/// (see the DELETE/PUT index-before-data ordering invariant); do not assume
/// atomic cross-keyspace visibility and "optimise away" that ordering.
/// Amortizes TCP round-trip, parse, lobe resolve, LINK resolve, and commit.
///
/// # Batch size
///
/// A batch is capped at `MAX_BATCH_SIZE` (10,000) records. A larger batch is
/// rejected whole with `XyzError::InvalidQuery` — no partial apply, no
/// truncation. To load more, split into chunks of <= 10,000; each chunk commits
/// as its own atomic batch (atomicity is per-chunk, never across chunks). The
/// bench/bulk loaders chunk at 5,000; a 0.8 migration tool must chunk too.
pub fn execute_put_batch(engine: &Engine, stmt: PutBatchStmt) -> Result<QueryResult> {
    let n = stmt.records.len();
    if n == 0 {
        return Ok(QueryResult::Ok {
            lid: None,
            message: "0 records inserted (empty batch)".into(),
        });
    }
    if n > MAX_BATCH_SIZE {
        return Err(XyzError::InvalidQuery(format!(
            "Batch too large: {n} records (max {MAX_BATCH_SIZE})"
        )));
    }

    // 1. Resolve lobe ONCE
    let lobe_id = {
        let mut lobes = engine.lobe_registry.write();
        let id = lobes.get_or_create(&stmt.lobe, None);
        engine.persist_lobe_registry(&lobes)?;
        id
    };

    // 2. Resolve LINK target ONCE (gravity_hash for all records)
    let linked_gravity_hash = if let Some(ref link_clause) = stmt.link {
        Some(resolve_gravity_hash_from_link(
            engine,
            &link_clause.target,
            &link_clause.filters,
        )?)
    } else {
        None
    };

    // 3. Build all records, check anchors, prepare batch
    let anchors = engine.anchor_registry.read();
    let anchor_fields = anchors.get_anchors(&stmt.lobe).clone();
    drop(anchors);

    let now_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    let mut batch = engine.turba.batch();
    let mut first_lid: Option<LID> = None;
    let mut last_lid: Option<LID> = None;
    // Collect (record, spatial_bytes) for ghost post-write hook
    let has_ghosts = !engine.ghost_manager.is_empty();
    let mut ghost_notify_buf: Vec<(Record, Vec<u8>)> = if has_ghosts {
        Vec::with_capacity(n)
    } else {
        Vec::new()
    };

    // 2b-bulk (b): acquire the write shards for EVERY anchor in the batch and
    // hold them across the per-record checks AND the commit below, so a
    // concurrent PUT/bulk to the same anchor cannot interleave (cross-batch
    // TOCTOU). `lock_anchor_shards` sorts + dedups the shard indices, so two
    // batches with overlapping shards acquire them in the same order — no
    // deadlock (8d only covered the single-shard PUT path). Single-stream bulk
    // sees no contention. Lightweight pass over the raw fields; released at
    // end of scope (after commit).
    let batch_anchor_dict_keys: Vec<Vec<u8>> = {
        let mut keys = Vec::new();
        for fp in &stmt.records {
            for name in &anchor_fields {
                if let Some(p) = fp.iter().find(|p| p.name == *name) {
                    let val_str = value_to_anchor_string(&literal_to_value(&p.value));
                    keys.push(dictionary_key(lobe_id, name, &val_str));
                }
            }
        }
        keys
    };
    let _batch_anchor_guards = engine.lock_anchor_shards(&batch_anchor_dict_keys);

    // 2b-bulk (a): anchor dict_keys already claimed by an EARLIER record in
    // THIS batch. The per-record dict.get cannot see in-flight records, so a
    // repeated anchor within one batch is caught here, deterministically.
    let mut seen_anchors: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    // Snapshot the lobe's gravity spec once (constant across the batch): a
    // declared Normalized/Composite spec routes placement through the keel so
    // batch writes land in the buckets the SCAN fast path reads. For a fresh
    // lobe (None) a single `*` folds identically via the fallback, and the
    // first record's auto-register persists Raw for subsequent reads.
    let gravity_spec = engine.get_gravity_spec(&stmt.lobe);

    // V4 hoist: snapshot the lobe's searchable vector field once (per-lobe
    // constant). When present, hold the field-registry write lock for the whole
    // loop and serialize matching records V3 through it — re-locking per record
    // would thrash; a single guard avoids that and any self-deadlock. When the
    // lobe has no vector spec, `vec_search` is None and every record stays on
    // the existing V1 `serialize_record` path, byte-identical to before.
    let vec_search = engine.get_vector_spec(&stmt.lobe).map(|s| s.field);
    let mut fr_guard = vec_search.as_ref().map(|_| engine.field_registry.write());

    for (idx, field_pairs) in stmt.records.iter().enumerate() {
        // Build fields map, collect gravity, inject _type
        let mut fields = BTreeMap::new();
        let mut has_type = false;
        let mut gravity_fields: Vec<(String, Value)> = Vec::new();
        for pf in field_pairs {
            if pf.name == "_type" {
                has_type = true;
            }
            let val = literal_to_value(&pf.value);
            if pf.gravity {
                gravity_fields.push((pf.name.clone(), val.clone()));
            }
            fields.insert(pf.name.clone(), val);
        }
        if !has_type {
            fields.insert("_type".into(), Value::Text(stmt.lobe.clone()));
        }

        // 3d-4: same multi-`*` guard as the single PUT path — a record marking
        // 2+ gravity fields needs a declared composite spec.
        if gravity_fields.len() >= 2 && !matches!(gravity_spec, Some(GravitySpec::Composite(_))) {
            return Err(multi_gravity_marker_error(&stmt.lobe, &gravity_fields));
        }

        // Finding 13: register lobe's gravity field on the first record carrying
        // a single `*` marker. After the first record, the read-lock fast path
        // makes subsequent calls effectively free (no contention, no
        // persistence).
        if let [(gf_name, _)] = gravity_fields.as_slice() {
            engine.register_gravity_field(&stmt.lobe, gf_name)?;
        }

        // Check anchors
        for anchor_name in &anchor_fields {
            if let Some(field_val) = fields.get(anchor_name) {
                let val_str = value_to_anchor_string(field_val);
                let dict_key = dictionary_key(lobe_id, anchor_name, &val_str);
                // 2b-bulk (a): a repeat of this anchor within the same batch —
                // same DuplicateAnchor as the PUT path (the dict.get below
                // cannot see the earlier in-flight record).
                if !seen_anchors.insert(dict_key.clone()) && stmt.on_conflict.is_none() {
                    return Err(XyzError::DuplicateAnchor {
                        anchor: anchor_name.clone(),
                        value: val_str.clone(),
                        lobe: stmt.lobe.clone(),
                        existing_lid: "batch".into(),
                    });
                }
                if let Some(_existing) = engine
                    .turba
                    .dictionary
                    .get(&dict_key)
                    .map_err(|e| XyzError::Storage(format!("dictionary get: {e}")))?
                {
                    if stmt.on_conflict.is_none() {
                        return Err(XyzError::DuplicateAnchor {
                            anchor: anchor_name.clone(),
                            value: val_str,
                            lobe: stmt.lobe.clone(),
                            existing_lid: "batch".into(),
                        });
                    }
                    // ON CONFLICT in batch: skip this record (simplified)
                    continue;
                }
            }
        }

        // Entity hash: LINK > gravity > anchor > fallback. Route through the
        // snapshot spec (the keel) so a declared Normalized/Composite lobe
        // places batch records in the same buckets the SCAN fast path reads.
        // Keel-omit health: count PER RECORD (a batch of N counts N, not 1 —
        // batch is the high-volume write path, so per-batch would dilute the
        // ratio exactly where it matters). Spec-declared lobes only; LINK excluded
        // (co-locates by the parent). Same criterion as the single-PUT path.
        if linked_gravity_hash.is_none()
            && let Some(ref s) = gravity_spec
        {
            engine.observe_keel(&stmt.lobe, s, s.compute_hash(&fields).is_some());
        }
        let gravity_hash = if let Some(leh) = linked_gravity_hash {
            leh
        } else if !gravity_fields.is_empty() {
            gravity_spec
                .as_ref()
                .and_then(|spec| spec.compute_hash(&fields))
                .unwrap_or_else(|| compute_gravity_hash(&gravity_fields))
        } else {
            // Honor a declared GRAVITY BY for a PLAIN field (no `*`) here too,
            // exactly like the single-PUT path (efbe49e). Without this, a plain
            // gravity field written via PUT BATCH silently fails to co-locate and
            // a scoped `WHERE <field> = X` finds nothing (the fast path scans the
            // keel bucket the record never landed in). Case C (field absent) still
            // returns None → the same anchor/LID fallback as before.
            gravity_spec
                .as_ref()
                .and_then(|spec| spec.compute_hash(&fields))
                .unwrap_or_else(|| compute_record_gravity_hash(engine, &stmt.lobe, &fields))
        };

        // Generate LID
        let lid = LID::new(lobe_id);
        if first_lid.is_none() {
            first_lid = Some(lid);
        }
        last_lid = Some(lid);

        // Spatial key with incrementing timestamp for ordering within batch
        let ts = normalize_timestamp(now_micros + idx as u64);
        let type_id = type_id_from_fields(&fields);
        let seq = RECORD_SEQ.fetch_add(1, Ordering::Relaxed);
        let spatial_key = SpatialKey::new(lobe_id, gravity_hash, type_id, ts, seq);
        let spatial_bytes = spatial_key.to_bytes();

        // Record
        let record = Record {
            lid,
            lobe_name: stmt.lobe.clone(),
            fields,
            created_at: now_micros as i64,
            updated_at: now_micros as i64,
        };
        // Validate (learn on first) the vector dimension before hoisting — a
        // mixed-dimension record in the batch (wrong model) is rejected here, not
        // silently dropped from NEAREST later. Different lock than `fr_guard`.
        if let Some(f) = &vec_search
            && let Some(Value::Vector(v)) = record.fields.get(f)
        {
            engine.ensure_vector_dim(&stmt.lobe, f, v.len())?;
        }
        // V5 split when the lobe has a searchable vector AND this record carries
        // it; otherwise the unchanged V1 path. The vector goes to the `vectors`
        // column under the same spatial key. `reg_entry` (the field-id→name
        // mapping, when the dict grew) co-commits in this same batch.
        let (record_bytes, vec_column, vec_reg_entry) = match (&mut fr_guard, &vec_search) {
            (Some(fr), Some(f)) if matches!(record.fields.get(f), Some(Value::Vector(_))) => {
                fr.serialize_record_v5_durable(lobe_id, &record, Some(f))?
            }
            _ => (serialize_record(&record), None, None),
        };

        // Add to batch
        batch.put_spatial(spatial_bytes.as_slice(), record_bytes.as_slice());
        batch.put_identity(lid.to_bytes().as_slice(), spatial_bytes.as_slice());
        if let Some(col) = &vec_column {
            batch.put_vectors(spatial_bytes.as_slice(), col.as_slice());
        }
        if let Some((k, v)) = &vec_reg_entry {
            batch.put_dictionary(k.as_slice(), v.as_slice());
        }

        // Anchor dictionary entries
        for anchor_name in &anchor_fields {
            if let Some(field_val) = record.fields.get(anchor_name) {
                let val_str = value_to_anchor_string(field_val);
                let dict_key = dictionary_key(lobe_id, anchor_name, &val_str);
                batch.put_dictionary(dict_key.as_slice(), lid.to_bytes().as_slice());
            }
        }

        // Save for ghost notification after commit
        if has_ghosts {
            ghost_notify_buf.push((record, spatial_bytes.to_vec()));
        }
    }

    // 4. Single atomic commit
    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("batch commit failed: {e}")))?;

    // 5. Ghost post-write hook for each record in the batch
    for (record, spatial_bytes) in &ghost_notify_buf {
        engine.ghost_manager.notify_write(
            lobe_id,
            record,
            spatial_bytes,
            crate::ghost::WriteType::Insert,
        );
    }

    // 6. Record writes in ghost router for staleness tracking + persist periodically
    if let Some(router) = engine.ghost_routers.read().get(&lobe_id) {
        router.record_writes(n as u64);
        // Persist every 10K writes to survive restarts
        if router.total_writes() % 10_000 < n as u64 {
            engine.persist_total_writes();
        }
    }

    // 6. Create link records if needed
    // Note: individual _link_ fields on batch records are skipped for performance.
    // Co-location via gravity_hash inheritance ensures PULL finds them via range scan.

    let fl = first_lid.unwrap_or_else(|| LID::from_raw(0));
    let ll = last_lid.unwrap_or_else(|| LID::from_raw(0));

    Ok(QueryResult::BatchOk {
        count: n,
        first_lid: fl,
        last_lid: ll,
    })
}

// ─── V5: Binary Bulk Insert (Protocol V3) ─────────────────────────────────

/// A pre-parsed record for V3 bulk insert. Fields + gravity already deserialized by the driver.
pub struct BulkRecord {
    pub fields: BTreeMap<String, Value>,
    pub gravity_fields: Vec<(String, Value)>,
}

/// Result of a bulk insert batch.
pub struct BulkInsertResult {
    pub count: u32,
    pub first_lid: LID,
    pub last_lid: LID,
}

/// Insert a batch of pre-parsed records into a lobe. Bypasses xyTalk parsing.
/// Used by Protocol V3. No anchor checking (bulk loads are assumed clean).
pub fn execute_bulk_insert(
    engine: &Engine,
    lobe_name: &str,
    records: Vec<BulkRecord>,
) -> Result<BulkInsertResult> {
    if records.is_empty() {
        return Ok(BulkInsertResult {
            count: 0,
            first_lid: LID::from_raw(0),
            last_lid: LID::from_raw(0),
        });
    }

    let lobe_id = {
        let mut lobes = engine.lobe_registry.write();
        let id = lobes.get_or_create(lobe_name, None);
        engine.persist_lobe_registry(&lobes)?;
        id
    };

    let now_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    let mut batch = engine.turba.batch();
    let mut first_lid: Option<LID> = None;
    let mut last_lid: Option<LID> = None;
    let n = records.len();
    // Collect for ghost hook only if ghosts exist
    let has_ghosts = !engine.ghost_manager.is_empty();
    let mut ghost_notify_buf: Vec<(Record, Vec<u8>)> = if has_ghosts {
        Vec::with_capacity(n)
    } else {
        Vec::new()
    };

    // Route placement through the lobe's gravity spec (the keel), like the
    // xyTalk PUT paths, so a declared Normalized/Composite lobe loaded via the
    // binary bulk path lands in the buckets the SCAN fast path reads. Snapshot
    // once (per-lobe constant). The bulk path does not auto-register Finding 13:
    // an undeclared lobe keeps the raw fallback (correct placement, no fast
    // path) as before.
    let gravity_spec = engine.get_gravity_spec(lobe_name);

    // V5 split for bulk-loaded vector lobes: a declared searchable vector means
    // each carrying record serializes V5 (blob without the vector) plus a column
    // entry, exactly like the xyTalk PUT paths — otherwise NEAREST on bulk data
    // would not benefit from the column. The field-registry write lock is held
    // for the whole batch (a per-lobe constant); an undeclared lobe stays V1 and
    // never touches the registry.
    let vec_search = engine.get_vector_spec(lobe_name).map(|s| s.field);
    let mut fr_guard = vec_search.as_ref().map(|_| engine.field_registry.write());

    for (idx, bulk_rec) in records.into_iter().enumerate() {
        let mut fields = bulk_rec.fields;

        // Inject _type if missing
        if !fields.contains_key("_type") {
            fields.insert("_type".into(), Value::Text(lobe_name.to_string()));
        }

        // Keel-omit health: count PER RECORD on the binary bulk path too (no
        // LINK clause here). Spec-declared lobes only.
        if let Some(ref s) = gravity_spec {
            engine.observe_keel(lobe_name, s, s.compute_hash(&fields).is_some());
        }
        // Entity hash from gravity (via the keel) or fallback
        let gravity_hash = if !bulk_rec.gravity_fields.is_empty() {
            if bulk_rec.gravity_fields.len() >= 2
                && !matches!(gravity_spec, Some(GravitySpec::Composite(_)))
            {
                return Err(multi_gravity_marker_error(
                    lobe_name,
                    &bulk_rec.gravity_fields,
                ));
            }
            gravity_spec
                .as_ref()
                .and_then(|spec| spec.compute_hash(&fields))
                .unwrap_or_else(|| compute_gravity_hash(&bulk_rec.gravity_fields))
        } else {
            // Honor a declared GRAVITY BY for a plain field on the binary bulk
            // path too, like the single-PUT / batch paths. Case C (field absent)
            // still returns None → the content-hash fallback below.
            gravity_spec
                .as_ref()
                .and_then(|spec| spec.compute_hash(&fields))
                .unwrap_or_else(|| {
                    let fallback = format!("{:?}", fields);
                    hash_to_48bits(&fallback)
                })
        };

        let lid = LID::new(lobe_id);
        if first_lid.is_none() {
            first_lid = Some(lid);
        }
        last_lid = Some(lid);

        let ts = normalize_timestamp(now_micros + idx as u64);
        let type_id = type_id_from_fields(&fields);
        let seq = RECORD_SEQ.fetch_add(1, Ordering::Relaxed);
        let spatial_key = SpatialKey::new(lobe_id, gravity_hash, type_id, ts, seq);
        let spatial_bytes = spatial_key.to_bytes();

        let record = Record {
            lid,
            lobe_name: lobe_name.to_string(),
            fields,
            created_at: now_micros as i64,
            updated_at: now_micros as i64,
        };

        // Validate (learn on first) the vector dimension before hoisting, same as
        // the xyTalk PUT paths — a wrong-dimension bulk record is rejected, not
        // silently unsearchable.
        if let Some(f) = &vec_search
            && let Some(Value::Vector(v)) = record.fields.get(f)
        {
            engine.ensure_vector_dim(lobe_name, f, v.len())?;
        }
        let (record_bytes, vec_column, vec_reg_entry) = match (&mut fr_guard, &vec_search) {
            (Some(fr), Some(f)) if matches!(record.fields.get(f), Some(Value::Vector(_))) => {
                fr.serialize_record_v5_durable(lobe_id, &record, Some(f))?
            }
            _ => (serialize_record(&record), None, None),
        };

        batch.put_spatial(spatial_bytes.as_slice(), record_bytes.as_slice());
        batch.put_identity(lid.to_bytes().as_slice(), spatial_bytes.as_slice());
        if let Some(col) = &vec_column {
            batch.put_vectors(spatial_bytes.as_slice(), col.as_slice());
        }
        if let Some((k, v)) = &vec_reg_entry {
            batch.put_dictionary(k.as_slice(), v.as_slice());
        }

        // Save for ghost notification after commit
        if has_ghosts {
            ghost_notify_buf.push((record, spatial_bytes.to_vec()));
        }
    }

    batch
        .commit()
        .map_err(|e| XyzError::Storage(format!("bulk insert commit: {e}")))?;

    // Ghost post-write hook for each record in the bulk batch
    for (record, spatial_bytes) in &ghost_notify_buf {
        engine.ghost_manager.notify_write(
            lobe_id,
            record,
            spatial_bytes,
            crate::ghost::WriteType::Insert,
        );
    }

    // Persist field registry if dirty
    // Record writes for ghost staleness tracking
    if let Some(router) = engine.ghost_routers.read().get(&lobe_id) {
        router.record_writes(n as u64);
    }

    let fl = first_lid.unwrap_or_else(|| LID::from_raw(0));
    let ll = last_lid.unwrap_or_else(|| LID::from_raw(0));

    Ok(BulkInsertResult {
        count: n as u32,
        first_lid: fl,
        last_lid: ll,
    })
}

#[cfg(test)]
mod gravity_dict_tests {
    use crate::engine::Engine;

    fn exec(engine: &Engine, s: &str) {
        engine
            .run(s)
            .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"));
    }

    /// 0.7.5: PUT must not write per-value gravity dictionary entries
    /// (0xFE prefix). They were 1-LID snapshots of an N-record bucket —
    /// wrong for FIND — and DELETE never cleaned them (unbounded growth
    /// under churn, since N-membership makes them un-refcountable).
    /// In-crate test because the keyspace handle is pub(crate).
    #[test]
    fn put_and_delete_leave_no_gravity_dictionary_entries() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        exec(&engine, r#"LOBE "creditos""#);
        for i in 0..20 {
            exec(
                &engine,
                &format!(r#"PUT {{_type: "Credit", *cliente_id: "C{i}", n: {i}}} IN "creditos""#),
            );
        }
        exec(&engine, r#"DELETE "creditos" WHERE n = 7"#);

        let leaked = engine
            .turba
            .dictionary
            .prefix_iter(&[0xFE])
            .expect("dictionary iter")
            .count();
        assert_eq!(
            leaked, 0,
            "no 0xFE gravity entries may exist in the dictionary keyspace"
        );
    }
}
