// SPDX-License-Identifier: BUSL-1.1
use crate::anchor::dictionary_key;
use crate::cursor::{
    CURSOR_FORMAT_V2, CursorPayload, decode_cursor, encode_cursor, filter_checksum,
};
use crate::engine::{Engine, QueryResult};
use crate::ops::{convert_filters, literal_to_string};
use xytalk_parser::ast::{Filter, FilterExpr, FindStmt, FindTarget};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::{SPATIAL_KEY_SIZE, SpatialKey};
use xyzdb_core::lid::LID;
use xyzdb_core::record::Record;

pub fn execute_find(engine: &Engine, stmt: FindStmt) -> Result<QueryResult> {
    // Cursor presence forces the paginated path (subsequent page).
    if stmt.cursor.is_some() {
        return execute_find_paginated(engine, &stmt);
    }
    // LIMIT presence on a gravity-eligible predicate triggers the
    // first-page paginated path. Anchor lookup with LIMIT is a no-op
    // (single record returned regardless); fall through to the
    // regular dispatch in that case.
    if stmt.limit.is_some()
        && let Some(result) = try_first_page_paginated(engine, &stmt)?
    {
        return Ok(result);
    }
    // Existing path: anchor -> gravity -> scan, no pagination.
    let results = resolve_find_internal(engine, &stmt.target, &stmt.filters)?;
    let records: Vec<Record> = results.into_iter().map(|(r, _)| r).collect();
    Ok(QueryResult::Records(records))
}

/// First-page entry point for paginated FIND. Returns `Some` only
/// when the predicate is on the gravity-bounded fast path — anchor
/// lookups, FIND LID, and no-fast-path predicates fall through to
/// the regular dispatch (returning `Ok(None)` here means "I didn't
/// take it; the caller should use the legacy path"). Cursor is
/// forbidden on this entry; if present the caller routes to
/// `execute_find_paginated` instead.
fn try_first_page_paginated(engine: &Engine, stmt: &FindStmt) -> Result<Option<QueryResult>> {
    debug_assert!(
        stmt.cursor.is_none(),
        "first-page entry must have no cursor"
    );
    debug_assert!(stmt.limit.is_some(), "first-page entry requires LIMIT");

    // FIND LID(...) — single-record; LIMIT is a no-op, fall through.
    let lobe_name = match &stmt.target {
        FindTarget::ByLid(_) => return Ok(None),
        FindTarget::Lobe(n) => n.clone(),
    };

    let lobes = engine.lobe_registry.read();
    let Some(lobe_config) = lobes.get(&lobe_name) else {
        // Lobe missing: fall through; the legacy path returns
        // LobeNotFound with the standard error shape.
        return Ok(None);
    };
    let lobe_id = lobe_config.id;
    drop(lobes);

    // Anchor-eq -> single record, LIMIT no-op, fall through.
    let anchors = engine.anchor_registry.read();
    let has_anchor_eq = stmt.filters.iter().any(|f| {
        f.op == xytalk_parser::ast::FilterOp::Eq && anchors.is_anchor(&lobe_name, &f.field)
    });
    drop(anchors);
    if has_anchor_eq {
        return Ok(None);
    }

    // Gravity-eq -> first-page paginated.
    let core_filters = convert_filters(&stmt.filters);
    let ghash = match crate::ops::scan::detect_gravity_eq(engine, &lobe_name, &core_filters) {
        Some(h) => h,
        // No fast path -> fall through to regular scan.
        None => return Ok(None),
    };

    let filter_expr = if stmt.filters.is_empty() {
        None
    } else {
        Some(FilterExpr::from_filters(stmt.filters.clone()))
    };
    let current_checksum = filter_checksum(&filter_expr);
    let effective_limit = stmt.limit.unwrap_or(crate::ops::scan::SCAN_LIMIT_DEFAULT);

    let out = find_gravity_paginated(engine, lobe_id, ghash, &filter_expr, effective_limit, None)?;

    let new_cursor = if out.has_more {
        let tail = out
            .page_tail_key
            .ok_or_else(|| XyzError::Internal("has_more without page tail".into()))?;
        Some(encode_cursor(&CursorPayload {
            format_ver: CURSOR_FORMAT_V2,
            lobe_id,
            last_spatial_key: tail,
            filter_checksum: current_checksum,
        })?)
    } else {
        None
    };

    Ok(Some(QueryResult::PaginatedRecords {
        records: out.records,
        cursor: new_cursor,
        has_more: out.has_more,
        budget_stop: None, // FIND never runs the NEAREST hydration airbag
    }))
}

/// v0.2.5.2 — Paginated FIND for the gravity-bounded fast path
/// (Finding 13). Cursor is rejected explicitly on shapes where it
/// cannot do useful work: anchor lookup (single record), FIND LID
/// (single record), and predicates with no anchor / no gravity.
fn execute_find_paginated(engine: &Engine, stmt: &FindStmt) -> Result<QueryResult> {
    let Some(token) = stmt.cursor.as_deref() else {
        // Caller (`execute_find`) routes to this function only when
        // `stmt.cursor.is_some()`. An empty cursor here means the
        // dispatch contract was violated.
        return Err(XyzError::Internal(
            "execute_find_paginated called without cursor".into(),
        ));
    };

    // FIND LID(...) — single-record lookup, cursor never applies.
    let lobe_name = match &stmt.target {
        FindTarget::ByLid(_) => {
            return Err(XyzError::InvalidQuery(
                "cursor not applicable to FIND LID(...); single-record lookup".into(),
            ));
        }
        FindTarget::Lobe(n) => n.clone(),
    };

    // Resolve the lobe early so we can verify the cursor's lobe binding.
    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(&lobe_name)
        .ok_or_else(|| XyzError::LobeNotFound(lobe_name.clone()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    // Reject anchor-only shapes — a single record can't paginate.
    let anchors = engine.anchor_registry.read();
    let has_anchor_eq = stmt.filters.iter().any(|f| {
        f.op == xytalk_parser::ast::FilterOp::Eq && anchors.is_anchor(&lobe_name, &f.field)
    });
    drop(anchors);
    if has_anchor_eq {
        return Err(XyzError::InvalidQuery(
            "cursor not applicable to anchor lookup; CURSOR is for paginated iteration. \
             An anchor returns at most 1 record."
                .into(),
        ));
    }

    // Detect gravity-eq fast path. Anything else is rejected — FIND
    // remains a fast-lookup verb; full-lobe iteration is SCAN.
    let core_filters = convert_filters(&stmt.filters);
    let ghash = match crate::ops::scan::detect_gravity_eq(engine, &lobe_name, &core_filters) {
        Some(h) => h,
        None => {
            return Err(XyzError::InvalidQuery(
                "field has no anchor or gravity; CURSOR is supported only on fast paths. \
                 Use SCAN for full-lobe iteration, or declare ANCHOR / register gravity."
                    .into(),
            ));
        }
    };

    // Decode + validate the cursor token.
    let payload = decode_cursor(token)?;
    if payload.lobe_id != lobe_id {
        return Err(XyzError::InvalidQuery(format!(
            "cursor invalid: token issued for lobe_id={}, current request targets lobe_id={lobe_id}",
            payload.lobe_id
        )));
    }
    // Reuse the SCAN cursor checksum format for cross-verb consistency.
    // FIND filters are AND-flat (Vec<Filter>), wrapped to FilterExpr.
    let filter_expr = if stmt.filters.is_empty() {
        None
    } else {
        Some(FilterExpr::from_filters(stmt.filters.clone()))
    };
    let current_checksum = filter_checksum(&filter_expr);
    if current_checksum != payload.filter_checksum {
        return Err(XyzError::InvalidQuery(
            "cursor invalid: WHERE clause does not match the cursor's binding; \
             cursors are only valid for the exact filter that produced them"
                .into(),
        ));
    }

    // Run the bounded range scan over the gravity bucket.
    let effective_limit = stmt.limit.unwrap_or(crate::ops::scan::SCAN_LIMIT_DEFAULT);
    let out = find_gravity_paginated(
        engine,
        lobe_id,
        ghash,
        &filter_expr,
        effective_limit,
        Some(&payload.last_spatial_key),
    )?;

    let new_cursor = if out.has_more {
        let tail = out
            .page_tail_key
            .ok_or_else(|| XyzError::Internal("has_more without page tail".into()))?;
        Some(encode_cursor(&CursorPayload {
            format_ver: CURSOR_FORMAT_V2,
            lobe_id,
            last_spatial_key: tail,
            filter_checksum: current_checksum,
        })?)
    } else {
        None
    };

    Ok(QueryResult::PaginatedRecords {
        records: out.records,
        cursor: new_cursor,
        has_more: out.has_more,
        budget_stop: None, // FIND never runs the NEAREST hydration airbag
    })
}

/// First-page FIND with cursor: detect gravity, run a bounded range
/// scan, and emit the next-page cursor when the bucket overflows the
/// active LIMIT. Sibling of `execute_find_paginated` for the
/// no-token entry case.
///
/// Currently invoked via the SCAN entry point — kept here for parity
/// with `execute_find_paginated`'s shape and because FIND owns the
/// "fast-path-only" rejection semantics for cursor.
#[allow(dead_code)]
struct FindPaginatedOutput {
    records: Vec<Record>,
    page_tail_key: Option<[u8; SPATIAL_KEY_SIZE]>,
    has_more: bool,
}

/// Bounded range scan over the gravity bucket of one specific
/// gravity-field value. Mirrors `scan_primary_gravity_indexed` from
/// `ops::scan` but extended with cursor seek + overscan-by-one for
/// `has_more` detection. The hash collision post-filter remains:
/// records that hashed to the same bucket but don't actually have
/// `field = value` are discarded by `record_matches_opt_expr`.
fn find_gravity_paginated(
    engine: &Engine,
    lobe_id: u16,
    gravity_hash: u64,
    filter_expr: &Option<FilterExpr>,
    effective_limit: u64,
    start_after: Option<&[u8]>,
) -> Result<FindPaginatedOutput> {
    let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, gravity_hash);

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);

    let mut records: Vec<Record> = Vec::new();
    let mut page_tail_key: Option<[u8; SPATIAL_KEY_SIZE]> = None;
    let mut has_more = false;
    let target = effective_limit.saturating_add(1);

    // Lower bound: cursor's last_spatial_key + 0x00 if resuming, else
    // the gravity bucket prefix. Upper bound: same gravity bucket end.
    let start_buf: Vec<u8>;
    let lower: &[u8] = match start_after {
        Some(k) => {
            start_buf = {
                let mut v = Vec::with_capacity(k.len() + 1);
                v.extend_from_slice(k);
                v.push(0x00);
                v
            };
            // Validate the cursor's resume point falls inside the bucket —
            // defends against a malformed token that somehow passed lobe_id +
            // checksum but points elsewhere. Compare the FULL resume key against
            // the bucket range: a real key in this (lobe_id, gravity_hash) bucket
            // sorts within [key_min, key_max]. (A former `[..10]` prefix compare
            // silently depended on bytes 8..10 being non-zero z_order; the 0.9.4
            // reserved sat axis made those bytes 0, so the prefix aliased key_min.)
            if k.len() != SPATIAL_KEY_SIZE || k < &key_min[..] || k > &key_max[..] {
                return Err(XyzError::InvalidQuery(
                    "cursor invalid: resume point falls outside the gravity bucket".into(),
                ));
            }
            &start_buf
        }
        None => key_min.as_slice(),
    };

    let entries = engine
        .turba
        .spatial
        .range_stream(lower, key_max.as_slice())
        .map_err(|e| XyzError::Storage(e.to_string()))?;

    for entry in entries {
        let key_bytes = &entry.key;
        let val = &entry.value;
        let Ok(record) = crate::ops::deserialize_hydrated(engine, key_bytes, val, &lobe_name, fd)
        else {
            continue;
        };
        if !crate::ops::record_matches_opt_expr(&record, filter_expr) {
            continue;
        }
        records.push(record);
        let len = records.len() as u64;
        if len <= effective_limit && key_bytes.len() == SPATIAL_KEY_SIZE {
            let mut k = [0u8; SPATIAL_KEY_SIZE];
            k.copy_from_slice(key_bytes);
            page_tail_key = Some(k);
        }
        if len >= target {
            records.truncate(effective_limit as usize);
            has_more = true;
            break;
        }
    }

    Ok(FindPaginatedOutput {
        records,
        page_tail_key,
        has_more,
    })
}

/// Unlimited FIND over one gravity bucket: bounded range scan keyed by
/// the gravity value's hash, FIND-shaped (records + spatial keys).
/// Collision-safe because `core_filters` carries the gravity Eq predicate
/// itself: records sharing the 48-bit bucket under a different value are
/// rejected by `matches_filters`.
fn find_gravity_bucket(
    engine: &Engine,
    lobe_id: u16,
    gravity_hash: u64,
    core_filters: &[(
        String,
        xyzdb_core::record::FilterOp,
        xyzdb_core::value::Value,
    )],
) -> Result<Vec<(Record, [u8; SPATIAL_KEY_SIZE])>> {
    let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, gravity_hash);

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let tree = engine.spatial_tree();

    let mut results = Vec::new();
    for entry in tree
        .range_stream(key_min.as_slice(), key_max.as_slice())
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        if entry.key.len() != SPATIAL_KEY_SIZE {
            continue;
        }
        let mut sk = [0u8; SPATIAL_KEY_SIZE];
        sk.copy_from_slice(&entry.key);
        if let Ok(record) =
            crate::ops::deserialize_hydrated(engine, &sk, &entry.value, &lobe_name, fd)
            && record.matches_filters(core_filters)
        {
            results.push((record, sk));
        }
    }
    Ok(results)
}

/// Internal: resolve a FindTarget + filters to matching records with their spatial keys.
pub(crate) fn resolve_find_internal(
    engine: &Engine,
    target: &FindTarget,
    filters: &[Filter],
) -> Result<Vec<(Record, [u8; SPATIAL_KEY_SIZE])>> {
    match target {
        FindTarget::ByLid(lid_str) => find_by_lid(engine, lid_str),
        FindTarget::Lobe(lobe_name) => find_in_lobe(engine, lobe_name, filters),
    }
}

/// FIND LID("...") — direct lookup via Identity keyspace (or RecordCache).
fn find_by_lid(engine: &Engine, lid_str: &str) -> Result<Vec<(Record, [u8; SPATIAL_KEY_SIZE])>> {
    let lid = LID::parse(lid_str)?;

    // V5: Check RecordCache first — avoids disk I/O entirely
    if let Some(cache) = &engine.record_cache {
        let lobe_id = lid.lobe_id();
        if let Some(record) = cache.get(lobe_id, &lid) {
            return Ok(vec![(record, [0u8; SPATIAL_KEY_SIZE])]);
        }
    }

    let lid_bytes = lid.to_bytes();

    let spatial_key_bytes = match engine
        .turba
        .identity
        .get(&lid_bytes)
        .map_err(|e| XyzError::Storage(format!("identity get: {e}")))?
    {
        Some(v) => v,
        None => return Ok(vec![]),
    };

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

    let record_bytes = match engine
        .turba
        .spatial
        .get(&sk_array)
        .map_err(|e| XyzError::Storage(format!("spatial get: {e}")))?
    {
        Some(v) => v,
        None => return Ok(vec![]),
    };

    let lobe_id = u16::from_be_bytes([sk_array[0], sk_array[1]]);
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let record =
        crate::ops::deserialize_hydrated(engine, &sk_array, &record_bytes, &lobe_name, fd)?;
    Ok(vec![(record, sk_array)])
}

/// FIND "lobe" WHERE field=value — try anchor path first, then full scan.
fn find_in_lobe(
    engine: &Engine,
    lobe_name: &str,
    filters: &[Filter],
) -> Result<Vec<(Record, [u8; SPATIAL_KEY_SIZE])>> {
    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(lobe_name)
        .ok_or_else(|| XyzError::LobeNotFound(lobe_name.into()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    // Try anchor-based lookup for the first filter that matches an anchor
    let anchors = engine.anchor_registry.read();
    for filter in filters {
        if filter.op == xytalk_parser::ast::FilterOp::Eq
            && anchors.is_anchor(lobe_name, &filter.field)
        {
            let val_str = literal_to_string(&filter.value);
            let dict_key = dictionary_key(lobe_id, &filter.field, &val_str);
            drop(anchors);

            if let Some(lid_bytes) = engine
                .turba
                .dictionary
                .get(&dict_key)
                .map_err(|e| XyzError::Storage(format!("dictionary get: {e}")))?
            {
                let lid_array: [u8; 16] = lid_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| XyzError::Internal("bad LID in dictionary".into()))?;
                let lid = LID::from_bytes(&lid_array);

                let results = find_by_lid(engine, &lid.to_string())?;

                // Apply remaining filters
                let core_filters = convert_filters(filters);
                let filtered: Vec<_> = results
                    .into_iter()
                    .filter(|(r, _)| r.matches_filters(&core_filters))
                    .collect();

                return Ok(filtered);
            } else {
                return Ok(vec![]);
            }
        }
    }
    drop(anchors);

    // Gravity fast path: a single Eq on the lobe's registered gravity
    // field routes to a bounded range scan over that value's bucket — the
    // same path SCAN (Finding 13) and paginated FIND already use. The
    // pre-0.7.5 gravity-dictionary lookup resolved (field, value) to a
    // SINGLE LID (each PUT overwrote the entry), so unlimited FIND
    // returned at most one of the bucket's N records.
    let core_filters = convert_filters(filters);
    if let Some(ghash) = crate::ops::scan::detect_gravity_eq(engine, lobe_name, &core_filters) {
        return find_gravity_bucket(engine, lobe_id, ghash, &core_filters);
    }

    // No anchor or gravity match — full scan of the lobe with filters.
    scan_lobe_filtered(engine, lobe_id, filters)
}

/// Full scan of a lobe, applying filters in memory.
///
/// Iterates the lobe's spatial Tree (`prefix_iter` over the lobe-id
/// prefix), deserializes each record, and returns those matching
/// `filters` together with their spatial key.
pub(crate) fn scan_lobe_filtered(
    engine: &Engine,
    lobe_id: u16,
    filters: &[Filter],
) -> Result<Vec<(Record, [u8; SPATIAL_KEY_SIZE])>> {
    let core_filters = convert_filters(filters);
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);

    // Build lobe prefix (first 2 bytes)
    let prefix = lobe_id.to_be_bytes();
    let mut results = Vec::new();
    let tree = engine.spatial_tree();

    for entry in tree
        .prefix_iter(&prefix)
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        let key_bytes = &entry.key;
        let val = &entry.value;

        if let Ok(record) = crate::ops::deserialize_hydrated(engine, key_bytes, val, &lobe_name, fd)
            && record.matches_filters(&core_filters)
        {
            let mut sk = [0u8; SPATIAL_KEY_SIZE];
            let copy_len = key_bytes.len().min(SPATIAL_KEY_SIZE);
            sk[..copy_len].copy_from_slice(&key_bytes[..copy_len]);
            results.push((record, sk));
        }
    }

    Ok(results)
}
