use crate::engine::{Engine, QueryResult};
use crate::ghost_router::ScanSource;
use crate::ops::convert_filters;
use crate::ops::nearest::BUDGET_CHECK_STRIDE;
use crate::scan_telemetry::{AutoGhostCandidate, ScanTelemetry};
use std::collections::BinaryHeap;
use std::time::Instant;
use xytalk_parser::ast::ScanStmt;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::SpatialKey;
use xyzdb_core::record::{FilterOp, Record};
use xyzdb_core::value::Value;

/// v0.2.5.1: default cap applied when SCAN omits LIMIT and ORDER BY.
/// Acts as a safety net against accidental full-lobe scans in production.
/// Override per-query with explicit `LIMIT N` (up to `SCAN_LIMIT_HARD_MAX`).
pub(crate) const SCAN_LIMIT_DEFAULT: u64 = 1_000;

/// v0.2.5.1: hard ceiling on `LIMIT N`. Larger result sets must be paginated
/// via `CURSOR` (forthcoming) or chunked-streaming SCAN (FORMAT_*_CHUNKED).
pub(crate) const SCAN_LIMIT_HARD_MAX: u64 = 10_000;

/// v0.2.5.1 LIMIT cap enforcement, shared across scan and aggregate paths.
fn validate_scan_limit_cap(stmt: &ScanStmt) -> Result<()> {
    if let Some(l) = stmt.limit
        && l > SCAN_LIMIT_HARD_MAX
    {
        return Err(XyzError::InvalidQuery(format!(
            "LIMIT {l} exceeds hard maximum {SCAN_LIMIT_HARD_MAX}; \
                 paginate with CURSOR or use chunked-streaming SCAN \
                 (V2 FORMAT_*_CHUNKED) for larger result sets"
        )));
    }
    Ok(())
}

/// v0.2.5.1 pagination guard for aggregate paths. CURSOR has no meaning
/// over an aggregate (you don't paginate a count or a sum), and silent
/// no-ops would be a footgun — surface an explicit error instead.
fn validate_no_cursor_for_aggregate(stmt: &ScanStmt) -> Result<()> {
    if stmt.cursor.is_some() {
        return Err(XyzError::InvalidQuery(
            "CURSOR pagination is not supported on aggregate pipelines; \
             aggregates collapse a SCAN to a single row (or per-group row)"
                .into(),
        ));
    }
    Ok(())
}

/// Result of execute_scan, including optional auto-ghost candidate.
pub struct ScanResult {
    pub query_result: QueryResult,
    pub auto_ghost: Option<AutoGhostCandidate>,
}

/// Cap policy for a SCAN (M2.1). `Capped` is the default safety net
/// (`SCAN_LIMIT_*`); `NearestUncapped` is the explicit, localized opt-out used
/// ONLY when a SCAN feeds a NEAREST — it lifts the IMPLICIT default cap on the
/// gravity-indexed bucket path so the exact top-k sees every candidate, bounded
/// by the budget airbag checked during iteration. An explicit `LIMIT` is still
/// honoured; residual / full-lobe scans stay capped (pending M2.3).
pub(crate) enum ScanCap {
    Capped,
    NearestUncapped { budget_ms: u64 },
}

/// Execute SCAN with the default safety cap — the public entry point. Every
/// caller that is NOT feeding a NEAREST stays capped (default-capped invariant).
pub fn execute_scan(engine: &Engine, stmt: ScanStmt) -> Result<ScanResult> {
    execute_scan_inner(engine, stmt, ScanCap::Capped)
}

/// Execute a SCAN that feeds a NEAREST: the gravity-indexed bucket scan is
/// uncapped (exact top-k needs the whole bucket) and time-bounded by `budget_ms`,
/// checked DURING iteration so the airbag bites while the bucket materializes.
pub(crate) fn execute_scan_for_nearest(
    engine: &Engine,
    stmt: ScanStmt,
    budget_ms: u64,
) -> Result<ScanResult> {
    execute_scan_inner(engine, stmt, ScanCap::NearestUncapped { budget_ms })
}

/// Execute SCAN: iterate records in a lobe, applying WHERE filters.
/// Uses the ghost router to decide between primary and ghost keyspaces.
fn execute_scan_inner(engine: &Engine, stmt: ScanStmt, cap: ScanCap) -> Result<ScanResult> {
    use crate::ops::{convert_filter_expr, filter_expr_to_flat};

    let scan_start = Instant::now();

    validate_scan_limit_cap(&stmt)?;

    if stmt.cursor.is_some() && stmt.order_by.is_some() {
        return Err(XyzError::InvalidQuery(
            "CURSOR with ORDER BY is not supported in v0.2.5.1; \
             paginated sort lands in v0.3 with a dedicated cursor variant"
                .into(),
        ));
    }

    // v0.2.5.1: cursor path — decode token, verify lobe + filter checksum,
    // force ScanSource::Primary, do bounded range scan with overscan.
    if let Some(token) = stmt.cursor.clone() {
        return execute_cursor_scan(engine, stmt, &token, scan_start);
    }

    if stmt.order_by.is_some() && stmt.limit.is_none() {
        return Err(XyzError::InvalidQuery(
            "ORDER BY requires LIMIT (sorting unbounded results is not supported)".into(),
        ));
    }

    // v0.2.5.1: default LIMIT applied when SCAN omits both LIMIT and ORDER BY.
    // ORDER BY paths set limit post-sort (handled by the topn helpers); their
    // LIMIT mandate is enforced above. Plain SCAN without LIMIT used to scan
    // the entire lobe — now capped at SCAN_LIMIT_DEFAULT with a tracing warn.
    let scan_limit = if stmt.order_by.is_some() {
        None
    } else if stmt.limit.is_some() {
        stmt.limit
    } else {
        tracing::warn!(
            "SCAN on lobe '{}' has no LIMIT clause; applying default cap of {} \
             records. Add explicit 'LIMIT N' (up to {}) or paginate with CURSOR.",
            stmt.lobe,
            SCAN_LIMIT_DEFAULT,
            SCAN_LIMIT_HARD_MAX
        );
        Some(SCAN_LIMIT_DEFAULT)
    };

    // M2.1: a NEAREST-feeding scan lifts the IMPLICIT default cap on the
    // gravity-indexed bucket path (exact top-k needs every candidate) and bounds
    // it with the budget airbag. An explicit `LIMIT` still applies; the
    // non-gravity (residual / full-lobe) paths keep `scan_limit` capped.
    let (gravity_limit, gravity_budget) = match cap {
        ScanCap::Capped => (scan_limit, 0u64),
        ScanCap::NearestUncapped { budget_ms } => (
            if stmt.limit.is_some() {
                scan_limit
            } else {
                None
            },
            budget_ms,
        ),
    };

    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(&stmt.lobe)
        .ok_or_else(|| XyzError::LobeNotFound(stmt.lobe.clone()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    // Convert FilterExpr to flat AND filters for backward-compat paths (ghost, router)
    let flat_filters = filter_expr_to_flat(&stmt.filter_expr);
    let core_filters = convert_filter_expr(&stmt.filter_expr);
    let filter_desc = format!("{:?}", &stmt.filter_expr);

    let router_filters = core_filters.clone();

    // Ghost routing. The router first tries a filter_desc equality match
    // (OR / complex expressions with a matching auto-ghost); failing that,
    // it falls back to the flat (field, op, value) tuple match. Both paths
    // are gated inside plan_scan — callers no longer short-circuit OR.
    let order_by_info = stmt
        .order_by
        .as_ref()
        .map(|o| (o.field.as_str(), o.descending));
    // C-16 / Entry 22: feed anchor info to the router so an Eq predicate
    // on an anchored field overrides ghost selection in favor of Primary.
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let anchor_guard = engine.anchor_registry.read();
    let anchored = anchor_guard.get_anchors(&lobe_name);
    let routers = engine.ghost_routers.read();
    // `scan_source` is mutable so that a transparent fallback on a
    // mid-scan-evicted ghost can downgrade it to `Primary` for the
    // subsequent telemetry / router-side bookkeeping in this function.
    let mut scan_source = match routers.get(&lobe_id) {
        Some(router) if router.has_ghosts() => router.plan_scan(
            &router_filters,
            true,
            order_by_info,
            false,
            &[],
            stmt.limit.is_some(),
            stmt.filter_expr.as_ref(),
            Some(anchored),
        ),
        _ => ScanSource::Primary,
    };
    drop(routers);
    drop(anchor_guard);

    // Pagination / truncation metadata. The full-lobe (Primary, None) branch
    // sets a real resumable cursor here; every other capped route reports
    // truncation via the bool the match yields, converted after the match into a
    // signal-only `has_more` (cursor: None).
    let mut pagination: Option<(Option<String>, bool)> = None;

    // Each arm yields `(records, truncated)`. `truncated` is true only when the
    // engine clipped the result and more rows remain; ORDER BY arms carry an
    // explicit caller LIMIT (never silent) and yield false.
    let (records, truncated) = match (&scan_source, &stmt.order_by) {
        (ScanSource::Ghost(ghost_name), Some(_order)) => {
            tracing::info!("Router: scan routed to ghost '{}' (ordered)", ghost_name);
            let limit = stmt.limit.unwrap_or(100) as usize;
            let fr_guard = engine.field_registry.read();
            let fd = fr_guard.get_dict(lobe_id);
            let recs = match engine.ghost_manager.read_topn(
                ghost_name,
                limit,
                &flat_filters,
                &engine.turba.spatial,
                fd,
            ) {
                Ok(r) => r,
                Err(XyzError::GhostNotFound(name)) => {
                    // TOCTOU: router selected this ghost, but it was evicted
                    // (LRU / TTL / manual drop) between plan_scan and read_topn.
                    // Unregister the dead entry, downgrade to Primary, re-execute
                    // the equivalent primary path. User sees no error.
                    tracing::debug!(
                        "Router: ghost '{}' not found mid-scan; falling back to Primary (ordered)",
                        name
                    );
                    fallback_unregister_ghost(engine, lobe_id, &name);
                    scan_source = ScanSource::Primary;
                    drop(fr_guard);
                    let order = stmt.order_by.as_ref().unwrap();
                    // Finding 13 fast path also applies in fallback.
                    if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                        scan_primary_gravity_indexed_topn(
                            engine,
                            lobe_id,
                            ghash,
                            &stmt.filter_expr,
                            &order.field,
                            order.descending,
                            limit,
                        )?
                    } else {
                        scan_primary_topn_expr(
                            engine,
                            lobe_id,
                            &stmt.filter_expr,
                            &order.field,
                            order.descending,
                            limit,
                        )?
                    }
                }
                Err(other) => return Err(other),
            };
            // ORDER BY carries an explicit LIMIT (a caller's choice, never a
            // silent cap), so this route never signals truncation.
            (recs, false)
        }
        (ScanSource::Ghost(ghost_name), None) => {
            tracing::info!(
                "Router: scan routed to ghost '{}' (filter scan)",
                ghost_name
            );
            let fr_guard = engine.field_registry.read();
            let fd = fr_guard.get_dict(lobe_id);
            // Overscan the ghost read by one so a capped result surfaces as
            // truncation instead of a silently-clipped page.
            let probe = scan_limit.map(|l| l.saturating_add(1)).unwrap_or(u64::MAX) as usize;
            match engine.ghost_manager.read_topn(
                ghost_name,
                probe,
                &flat_filters,
                &engine.turba.spatial,
                fd,
            ) {
                Ok(mut r) => match scan_limit {
                    Some(lim) if r.len() as u64 > lim => {
                        r.truncate(lim as usize);
                        (r, true)
                    }
                    _ => (r, false),
                },
                Err(XyzError::GhostNotFound(name)) => {
                    tracing::debug!(
                        "Router: ghost '{}' not found mid-scan; falling back to Primary (filter)",
                        name
                    );
                    fallback_unregister_ghost(engine, lobe_id, &name);
                    scan_source = ScanSource::Primary;
                    drop(fr_guard);
                    if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                        scan_primary_gravity_indexed(
                            engine,
                            lobe_id,
                            ghash,
                            &stmt.filter_expr,
                            gravity_limit,
                            gravity_budget,
                        )?
                    } else {
                        scan_primary_full_expr(engine, lobe_id, &stmt.filter_expr, scan_limit)?
                    }
                }
                Err(other) => return Err(other),
            }
        }
        (ScanSource::GhostPreComputed(_), _) => {
            // PreComputed handled in execute_scan_aggregate/execute_scan_group_aggregate
            // If we reach here, it means a plain SCAN hit PreComputed — fall back to primary scan
            tracing::warn!("GhostPreComputed reached plain SCAN — unexpected");
            if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                scan_primary_gravity_indexed(
                    engine,
                    lobe_id,
                    ghash,
                    &stmt.filter_expr,
                    gravity_limit,
                    gravity_budget,
                )?
            } else {
                scan_primary_full_expr(engine, lobe_id, &stmt.filter_expr, scan_limit)?
            }
        }
        (ScanSource::Primary, Some(order)) => {
            let limit = stmt.limit.unwrap() as usize;
            // Finding 13: gravity-indexed fast path before falling through
            // to full-lobe topn scan. Detect single-Eq-on-gravity-field
            // shape; if matched, do bounded range scan + post-sort.
            let recs = if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                scan_primary_gravity_indexed_topn(
                    engine,
                    lobe_id,
                    ghash,
                    &stmt.filter_expr,
                    &order.field,
                    order.descending,
                    limit,
                )?
            } else {
                scan_primary_topn_expr(
                    engine,
                    lobe_id,
                    &stmt.filter_expr,
                    &order.field,
                    order.descending,
                    limit,
                )?
            };
            (recs, false)
        }
        (ScanSource::Primary, None) => {
            // Finding 13: gravity-indexed fast path before falling through
            // to full-lobe scan.
            if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                // Gravity-indexed fast path: the leaf overscans and reports
                // truncation. A NEAREST-feeding scan passes gravity_limit=None,
                // so it never overscans and never signals — it needs every
                // candidate.
                scan_primary_gravity_indexed(
                    engine,
                    lobe_id,
                    ghash,
                    &stmt.filter_expr,
                    gravity_limit,
                    gravity_budget,
                )?
            } else {
                // v0.2.5.1: paginated full-lobe SCAN with a real resumable
                // cursor. It sets `pagination` itself and yields `false`, so the
                // signal-only truncation below does not overwrite the cursor.
                let effective_limit = scan_limit.unwrap_or(SCAN_LIMIT_DEFAULT);
                let out = scan_primary_paginated(
                    engine,
                    lobe_id,
                    &stmt.filter_expr,
                    effective_limit,
                    None,
                )?;
                if out.has_more {
                    let tail = out.page_tail_key.ok_or_else(|| {
                        XyzError::Internal("has_more without page tail key".into())
                    })?;
                    let token = crate::cursor::encode_cursor(&crate::cursor::CursorPayload {
                        format_ver: crate::cursor::CURSOR_FORMAT_V2,
                        lobe_id,
                        last_spatial_key: tail,
                        filter_checksum: crate::cursor::filter_checksum(&stmt.filter_expr),
                    })?;
                    pagination = Some((Some(token), true));
                }
                (out.records, false)
            }
        }
    };

    // Universal truncation guarantee: any capped user SCAN (Capped mode) the
    // engine clipped surfaces as `has_more` with no resumable cursor — whichever
    // route served it (gravity fast path, ghost read, or full-expr fallback) — so
    // a partial result is never mistaken for a complete one. A NEAREST-feeding
    // scan (NearestUncapped) is internal and never signals; the full-lobe branch
    // set its own resumable cursor above and yielded `truncated == false`, so
    // this leaves that cursor intact.
    if truncated && matches!(cap, ScanCap::Capped) {
        pagination = Some((None, true));
    }

    let source_name = match &scan_source {
        ScanSource::Primary => "primary".to_string(),
        ScanSource::Ghost(name) => format!("ghost:{name}"),
        ScanSource::GhostPreComputed(name) => format!("ghost_pre:{name}"),
    };

    let duration = scan_start.elapsed();
    let telemetry = ScanTelemetry {
        lobe: stmt.lobe,
        filter_desc,
        source: source_name,
        records_scanned: 0,
        records_returned: records.len() as u64,
        duration,
    };

    // Telemetry is split by scan_source so ghost-routed scans can't
    // deflate pattern latency averages (Primary-only feeds the pattern
    // store) but stay visible in `SHOW SCAN STATS` (both paths feed
    // `recent`). Ghost reads also bump the ghost's in-memory access
    // tracking — the TTL reaper reads those fields.
    match &scan_source {
        ScanSource::Primary => {
            let auto_ghost =
                engine
                    .scan_telemetry
                    .write()
                    .record_with_filters(telemetry, &flat_filters, &[]);
            if let Some(c) = auto_ghost {
                engine.maybe_create_ephemeral_ghost(c);
            }
        }
        ScanSource::Ghost(name) | ScanSource::GhostPreComputed(name) => {
            engine.ghost_manager.bump_access(name);
            engine.scan_telemetry.write().record_routed(telemetry);
        }
    }

    let query_result = match pagination {
        Some((cursor, has_more)) => QueryResult::PaginatedRecords {
            records,
            cursor,
            has_more,
            budget_stop: None, // SCAN cap / cursor page, not a NEAREST budget stop
        },
        None => QueryResult::Records(records),
    };

    Ok(ScanResult {
        query_result,
        auto_ghost: None,
    })
}

/// Shared fallback for Finding 1 (TOCTOU transparent retry): when a
/// `ScanSource::Ghost` or `ScanSource::GhostPreComputed` target was
/// selected by the router but disappeared before the read path reached
/// it (LRU eviction / TTL / concurrent DROP GHOST), the read returns
/// `XyzError::GhostNotFound`. Callers catch that specifically and invoke
/// this helper to unregister the stale ghost from the router and clear
/// the telemetry flag — so subsequent scans of the same pattern neither
/// re-hit the dead entry nor stay blocked from re-triggering auto-create.
fn fallback_unregister_ghost(engine: &Engine, lobe_id: u16, ghost_name: &str) {
    // Capture filter_desc as an owned String before `unregister_ghost`
    // takes &mut router, so the telemetry clear below can match the
    // exact pattern that had been tagged `ghost_created`. Returns None
    // if the router already dropped it (e.g. another thread raced us
    // through the same fallback).
    let desc = {
        let mut routers = engine.ghost_routers.write();
        let Some(router) = routers.get_mut(&lobe_id) else {
            return;
        };
        let d = router.get_filter_desc(ghost_name).map(|s| s.to_string());
        router.unregister_ghost(ghost_name);
        d
    };
    if let Some(d) = desc {
        engine.scan_telemetry.write().set_ghost_flag(&d, false);
    }
}

/// Finding 13: detect the SCAN equality fast path. Returns
/// `Some(gravity_hash)` — the pinned gravity bucket hash (a `u64`) — when
/// `core_filters` pins the lobe's gravity spec (every gravity field carries
/// exactly one `Eq`); else `None` (caller falls back to the full Primary
/// scan). This resolves the bucket hash only; it does NOT itself scan.
///
/// Disqualifying conditions (return `None`):
/// - Lobe has no registered gravity field (PUTs without `*field` markers).
/// - The gravity field appears with a non-`Eq` operator (`!=`, `<`, etc.).
/// - The gravity field appears with multiple `Eq` predicates (impossible
///   to satisfy or redundant; conservatively fall back).
///
/// Other predicates on non-gravity fields are allowed and apply as
/// in-range filters after the bounded range scan; they do not disqualify.
pub(crate) fn detect_gravity_eq(
    engine: &Engine,
    lobe: &str,
    core_filters: &[(String, FilterOp, Value)],
) -> Option<u64> {
    // Route through the lobe's GravitySpec (the keel): it pins the bucket only
    // when every gravity field has exactly one Eq, and folds the value the same
    // way the write side did — so Raw is byte-identical to the pre-keel path,
    // and Normalized/Composite resolve to the bucket their writes landed in.
    engine
        .get_gravity_spec(lobe)?
        .pinned_gravity_hash(core_filters)
}

/// Finding 13: bounded range scan over the gravity bucket of one specific
/// gravity-field value. Replaces the full-lobe Primary scan when the
/// query has the single-Eq-on-gravity-field shape.
///
/// Hash collisions (21-bit hash space, ~2 M slots) are possible but rare.
/// The post-range filter via `record_matches_opt_expr` covers them: any
/// record that hashed to the same bucket but doesn't actually have
/// `field = value` is discarded. Correctness is preserved exactly.
fn scan_primary_gravity_indexed(
    engine: &Engine,
    lobe_id: u16,
    gravity_hash: u64,
    filter_expr: &Option<xytalk_parser::ast::FilterExpr>,
    limit: Option<u64>,
    budget_ms: u64,
) -> Result<(Vec<Record>, bool)> {
    let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, gravity_hash);

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let mut results = Vec::new();

    // M2.1 airbag: when uncapped (NEAREST), bound the bucket scan by wall-clock
    // here — DURING materialization — so a runaway aborts before the whole
    // `Vec<Entry>` is built, not after. `budget_ms == 0` (the capped default)
    // skips the guard; the cap bounds those scans instead.
    let started = Instant::now();
    let mut scanned = 0usize;
    for entry in engine
        .turba
        .spatial
        .range_stream(key_min.as_slice(), key_max.as_slice())
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        scanned += 1;
        if budget_ms > 0
            && scanned.is_multiple_of(BUDGET_CHECK_STRIDE)
            && started.elapsed().as_millis() as u64 >= budget_ms
        {
            return Err(XyzError::NearestBudgetExceeded { scanned, budget_ms });
        }
        let val = &entry.value;
        if let Ok(record) =
            crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
            && crate::ops::record_matches_opt_expr(&record, filter_expr)
        {
            results.push(record);
            // Overscan by one: fetch a single record past the cap to prove the
            // bucket overflows it. The extra is dropped and reported as
            // truncation so the caller can surface `has_more`.
            if let Some(lim) = limit
                && results.len() as u64 > lim
            {
                results.truncate(lim as usize);
                return Ok((results, true));
            }
        }
    }
    Ok((results, false))
}

/// Same fast path as `scan_primary_gravity_indexed` but with ORDER BY +
/// LIMIT applied via min-heap (matches `scan_primary_topn_expr` shape).
fn scan_primary_gravity_indexed_topn(
    engine: &Engine,
    lobe_id: u16,
    gravity_hash: u64,
    filter_expr: &Option<xytalk_parser::ast::FilterExpr>,
    field: &str,
    descending: bool,
    limit: usize,
) -> Result<Vec<Record>> {
    let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, gravity_hash);

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(limit + 1);

    for entry in engine
        .turba
        .spatial
        .range_stream(key_min.as_slice(), key_max.as_slice())
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        let val = &entry.value;
        if let Ok(record) =
            crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
            && crate::ops::record_matches_opt_expr(&record, filter_expr)
        {
            let sort_val = xyzdb_core::record::resolve_path(&record.fields, field).cloned();
            heap.push(HeapEntry {
                record,
                sort_val,
                invert: descending,
            });
            if heap.len() > limit {
                heap.pop();
            }
        }
    }
    let results: Vec<Record> = heap
        .into_sorted_vec()
        .into_iter()
        .map(|e| e.record)
        .collect();
    Ok(results)
}

fn scan_primary_full_expr(
    engine: &Engine,
    lobe_id: u16,
    filter_expr: &Option<xytalk_parser::ast::FilterExpr>,
    limit: Option<u64>,
) -> Result<(Vec<Record>, bool)> {
    let prefix = lobe_id.to_be_bytes();
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let mut results = Vec::new();

    // Zone map block filter: only evaluate on HDD where skipping a block saves 10ms+.
    // On SSD the deserialization overhead (~0.05ms/block) outweighs the skip benefit (~0.1ms).
    // Zone maps are still built during background compaction for future use.
    // TODO: make this configurable via storage profile instead of disabled
    // Block-filter factory type; a type alias is a design change, deferred (not a lint fix).
    #[allow(clippy::type_complexity)]
    let block_filter: Option<std::sync::Arc<dyn Fn(&[u8], usize) -> bool + Send + Sync>> = None;

    for entry in engine
        .spatial_tree()
        .prefix_iter_filtered(&prefix, block_filter)
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        let val = &entry.value;
        if let Ok(record) =
            crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
            && crate::ops::record_matches_opt_expr(&record, filter_expr)
        {
            results.push(record);
            // Overscan by one (see scan_primary_gravity_indexed): the extra
            // record past the cap is dropped and reported as truncation.
            if let Some(lim) = limit
                && results.len() as u64 > lim
            {
                results.truncate(lim as usize);
                return Ok((results, true));
            }
        }
    }

    Ok((results, false))
}

/// ORDER BY + LIMIT with FilterExpr.
fn scan_primary_topn_expr(
    engine: &Engine,
    lobe_id: u16,
    filter_expr: &Option<xytalk_parser::ast::FilterExpr>,
    field: &str,
    descending: bool,
    limit: usize,
) -> Result<Vec<Record>> {
    let prefix = lobe_id.to_be_bytes();
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(limit + 1);

    for entry in engine
        .spatial_tree()
        .prefix_iter(&prefix)
        .map_err(|e| XyzError::Storage(e.to_string()))?
    {
        let val = &entry.value;
        if let Ok(record) =
            crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
            && crate::ops::record_matches_opt_expr(&record, filter_expr)
        {
            let sort_val = xyzdb_core::record::resolve_path(&record.fields, field).cloned();
            heap.push(HeapEntry {
                record,
                sort_val,
                invert: descending,
            });
            if heap.len() > limit {
                heap.pop();
            }
        }
    }

    let results: Vec<Record> = heap
        .into_sorted_vec()
        .into_iter()
        .map(|e| e.record)
        .collect();
    Ok(results)
}

/// Wrapper for BinaryHeap that implements custom ordering.
struct HeapEntry {
    record: Record,
    sort_val: Option<Value>,
    /// When true, comparison is inverted (turns min-heap into max-heap).
    invert: bool,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        compare_values(&self.sort_val, &other.sort_val) == std::cmp::Ordering::Equal
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let cmp = compare_values(&self.sort_val, &other.sort_val);
        if self.invert { cmp.reverse() } else { cmp }
    }
}

/// Extract field names from aggregate metrics (for ghost projection).
fn extract_aggregate_fields(aggs: &[xytalk_parser::ast::Aggregate]) -> Vec<String> {
    aggs.iter()
        .filter_map(|a| match &a.func {
            xytalk_parser::ast::AggregateFunc::Count => None,
            xytalk_parser::ast::AggregateFunc::Sum(s)
            | xytalk_parser::ast::AggregateFunc::Avg(s)
            | xytalk_parser::ast::AggregateFunc::Min(s)
            | xytalk_parser::ast::AggregateFunc::Max(s) => Some(s.clone()),
        })
        .collect()
}

/// Incremental SCAN + AGGREGATE: iterates records and accumulates without
/// building a Vec<Record>. Memory: O(1) regardless of dataset size.
pub fn execute_scan_aggregate(
    engine: &Engine,
    stmt: ScanStmt,
    funcs: Vec<xytalk_parser::ast::Aggregate>,
) -> Result<QueryResult> {
    validate_no_cursor_for_aggregate(&stmt)?;
    // Reject ambiguous clauses (duplicate labels / filtered count without alias)
    // up front, so a malformed pipeline errors before any scan work.
    crate::ops::aggregate::resolve_labels(&funcs, crate::ops::aggregate::canonical_label)
        .map_err(XyzError::Parse)?;
    validate_scan_limit_cap(&stmt)?;

    let agg_start = Instant::now();
    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(&stmt.lobe)
        .ok_or_else(|| XyzError::LobeNotFound(stmt.lobe.clone()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    let flat_filters = crate::ops::filter_expr_to_flat(&stmt.filter_expr);
    let core_filters = convert_filters(&flat_filters);
    let router_filters = core_filters.clone();
    let filter_desc = format!("{:?}", &stmt.filter_expr);

    // C-16 / Entry 22: anchor-Eq override before ghost routing.
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let anchor_guard = engine.anchor_registry.read();
    let anchored = anchor_guard.get_anchors(&lobe_name);
    // Metric-match guard: a query routes to a PreComputed ghost only when that
    // ghost precomputes every metric the query asks for (same op/field/label/
    // filter). Otherwise it would return another metric's value under the wrong
    // name — so a mismatch falls back to the always-correct primary scan.
    let query_sig = Engine::funcs_to_metrics(&funcs)
        .map(|m| crate::aggregate_state::aggregate_signature(&m))
        .unwrap_or_default();
    // Check ghost routing (PreComputed first)
    let routers = engine.ghost_routers.read();
    let scan_source = match routers.get(&lobe_id) {
        Some(router) if router.has_ghosts() => {
            let src = router.plan_scan(
                &router_filters,
                true,
                None,
                true,
                &[],
                false,
                stmt.filter_expr.as_ref(),
                Some(anchored),
            );
            match &src {
                ScanSource::GhostPreComputed(name)
                    if !router.aggregate_sig_covers(name, &query_sig) =>
                {
                    tracing::info!(
                        "scan_aggregate: ghost '{}' metric signature does not cover the query; \
                         falling back to Primary",
                        name
                    );
                    ScanSource::Primary
                }
                _ => src,
            }
        }
        _ => ScanSource::Primary,
    };
    drop(routers);
    drop(anchor_guard);

    tracing::info!("scan_aggregate: scan_source = {:?}", scan_source);
    // PreComputed: return aggregates directly from metadata (zero scan)
    if let ScanSource::GhostPreComputed(ref ghost_name) = scan_source {
        tracing::info!(
            "Router: AGGREGATE routed to ghost PreComputed '{}'",
            ghost_name
        );
        match engine
            .ghost_manager
            .read_precomputed(ghost_name, &[], &router_filters)
        {
            Ok(crate::ghost::GhostAggregates::Global(state)) => {
                // Short-circuit returns here. Bump access + record for
                // diagnostics before returning, otherwise PreComputed
                // ghosts become invisible to SHOW SCAN STATS and never
                // tick their `last_accessed` for the TTL reaper.
                engine.ghost_manager.bump_access(ghost_name);
                engine.scan_telemetry.write().record_routed(ScanTelemetry {
                    lobe: stmt.lobe.clone(),
                    filter_desc: filter_desc.clone(),
                    source: format!("ghost_pre:{ghost_name}"),
                    records_scanned: 0,
                    records_returned: 0,
                    duration: agg_start.elapsed(),
                });
                let result: std::collections::BTreeMap<String, Value> =
                    state.to_result().into_iter().collect();
                return Ok(QueryResult::Aggregation(result));
            }
            Ok(_) => {} // Grouped returned for non-grouped request — fall through
            Err(XyzError::GhostNotFound(name)) => {
                // TOCTOU fallback (Finding 1): the precomputed ghost
                // disappeared between router select and read. Clean up
                // the router entry + telemetry flag so subsequent calls
                // don't keep hitting the dead name; continue to the
                // Primary scan below.
                tracing::debug!(
                    "AGGREGATE: PreComputed ghost '{}' not found mid-read; falling back to Primary",
                    name
                );
                fallback_unregister_ghost(engine, lobe_id, &name);
            }
            Err(other) => return Err(other),
        }
    }

    let agg_fields = extract_aggregate_fields(&funcs);
    let mut acc = crate::ops::aggregate::AggAccumulator::new(funcs);

    let limit = stmt.limit;

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);

    match scan_source {
        ScanSource::Primary | ScanSource::GhostPreComputed(_) | ScanSource::Ghost(_) => {
            let mut counted = 0u64;
            // Finding 13: gravity-indexed fast path also applies to AGGREGATE
            // (no ORDER BY here; scan over the bounded range, observe each).
            if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, ghash);
                for entry in engine
                    .turba
                    .spatial
                    .range_stream(key_min.as_slice(), key_max.as_slice())
                    .map_err(|e| XyzError::Storage(e.to_string()))?
                {
                    let val = &entry.value;
                    if let Ok(record) =
                        crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
                        && crate::ops::record_matches_opt_expr(&record, &stmt.filter_expr)
                    {
                        acc.observe(&record);
                        counted += 1;
                        if let Some(lim) = limit
                            && counted >= lim
                        {
                            break;
                        }
                    }
                }
            } else {
                let prefix = lobe_id.to_be_bytes();
                for entry in engine
                    .spatial_tree()
                    .prefix_iter(&prefix)
                    .map_err(|e| XyzError::Storage(e.to_string()))?
                {
                    let val = &entry.value;
                    if let Ok(record) =
                        crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
                        && crate::ops::record_matches_opt_expr(&record, &stmt.filter_expr)
                    {
                        acc.observe(&record);
                        counted += 1;
                        if let Some(lim) = limit
                            && counted >= lim
                        {
                            break;
                        }
                    }
                }
            }
        }
    }

    // Record telemetry with aggregate fields for ghost projection. Aggregate
    // scan patterns produce candidates with non-empty `aggregate_fields`, so
    // the auto-ghost worker builds a ghost with Count/Sum pre-computed per
    // field and has_aggregates=true — that ghost CAN route to PreComputed on
    // subsequent queries, short-circuiting the scan entirely.
    let filter_desc = format!("{:?}", &stmt.filter_expr);
    let telemetry = ScanTelemetry {
        lobe: stmt.lobe,
        filter_desc,
        source: match &scan_source {
            ScanSource::Primary => "primary".to_string(),
            ScanSource::Ghost(name) => format!("ghost:{name}"),
            ScanSource::GhostPreComputed(name) => format!("ghost_pre:{name}"),
        },
        records_scanned: 0,
        records_returned: 0,
        duration: agg_start.elapsed(),
    };

    match &scan_source {
        ScanSource::Primary => {
            let auto_ghost = engine.scan_telemetry.write().record_with_filters(
                telemetry,
                &flat_filters,
                &agg_fields,
            );
            if let Some(c) = auto_ghost {
                engine.maybe_create_ephemeral_ghost(c);
            }
        }
        ScanSource::Ghost(name) | ScanSource::GhostPreComputed(name) => {
            engine.ghost_manager.bump_access(name);
            engine.scan_telemetry.write().record_routed(telemetry);
        }
    }

    Ok(acc.finalize())
}

// ─── GROUP BY + AGGREGATE (streaming per-group accumulation) ─────────────

/// Execute SCAN | GROUP BY fields | AGGREGATE funcs.
/// Streaming: one accumulator per group, O(num_groups) memory.
pub fn execute_scan_group_aggregate(
    engine: &Engine,
    stmt: ScanStmt,
    group_fields: Vec<String>,
    funcs: Vec<xytalk_parser::ast::Aggregate>,
    top: Option<&xytalk_parser::ast::TopStmt>,
) -> Result<QueryResult> {
    use crate::ops::aggregate::{AggAccumulator, canonical_key};
    use xyzdb_core::record::resolve_path;

    validate_no_cursor_for_aggregate(&stmt)?;
    validate_scan_limit_cap(&stmt)?;
    // Validate the clause once for all groups (labels / filtered-count rule).
    crate::ops::aggregate::resolve_labels(&funcs, crate::ops::aggregate::canonical_label)
        .map_err(XyzError::Parse)?;

    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(&stmt.lobe)
        .ok_or_else(|| XyzError::LobeNotFound(stmt.lobe.clone()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    let flat_filters = crate::ops::filter_expr_to_flat(&stmt.filter_expr);
    let core_filters = convert_filters(&flat_filters);
    let router_filters: Vec<_> = core_filters
        .iter()
        .map(|(f, o, v)| (f.clone(), *o, v.clone()))
        .collect();

    // C-16 / Entry 22: anchor-Eq override before ghost routing.
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let anchor_guard = engine.anchor_registry.read();
    let anchored = anchor_guard.get_anchors(&lobe_name);
    // Metric-match guard (see execute_scan_aggregate): route to a PreComputed
    // ghost only when it precomputes every metric the query requests.
    let query_sig = Engine::funcs_to_metrics(&funcs)
        .map(|m| crate::aggregate_state::aggregate_signature(&m))
        .unwrap_or_default();
    let routers = engine.ghost_routers.read();
    let scan_source = match routers.get(&lobe_id) {
        Some(router) if router.has_ghosts() => {
            let src = router.plan_scan(
                &router_filters,
                true,
                None,
                true,
                &group_fields,
                false,
                stmt.filter_expr.as_ref(),
                Some(anchored),
            );
            match &src {
                ScanSource::GhostPreComputed(name)
                    if !router.aggregate_sig_covers(name, &query_sig) =>
                {
                    tracing::info!(
                        "scan_group_aggregate: ghost '{}' metric signature does not cover the \
                         query; falling back to Primary",
                        name
                    );
                    ScanSource::Primary
                }
                _ => src,
            }
        }
        _ => ScanSource::Primary,
    };
    drop(routers);
    drop(anchor_guard);

    // PreComputed: return grouped aggregates directly from metadata
    if let ScanSource::GhostPreComputed(ref ghost_name) = scan_source {
        // O(N) metric-order fast path: when the query is `… | TOP n BY <metric>`
        // AND this ghost declared `ORDER BY <metric>` (same metric + direction,
        // order emitted/fresh) AND it's a true global top-N (no group-field Eq
        // pin), read the first N straight from the metric-ordered rollup instead
        // of materialising all M groups. `read_topn_metric` returns None when the
        // ghost's order doesn't match or is stale, falling back to the O(M) path
        // below. The result is bit-identical (shared row builder; `apply_top`
        // re-checks the order downstream).
        // The metric-order fast path needs a BY metric to match against the
        // ghost's declared order; `TAKE n` (no BY) truncates and can't ride it.
        if let Some(top) = top
            && let Some(by) = &top.by
        {
            let all_wildcard = !group_fields.iter().any(|gf| {
                router_filters
                    .iter()
                    .any(|(f, op, _)| f == gf && *op == FilterOp::Eq)
            });
            if all_wildcard {
                let label = match by {
                    xytalk_parser::ast::TopBy::Metric(f) => {
                        crate::ops::aggregate::canonical_label(f)
                    }
                    xytalk_parser::ast::TopBy::Alias(a) => a.clone(),
                };
                if let Some(rows) = engine.ghost_manager.read_topn_metric(
                    ghost_name,
                    &group_fields,
                    &label,
                    top.descending,
                    top.n as usize,
                )? {
                    tracing::info!(
                        "Router: TOP served O(N) from metric-order of ghost '{}'",
                        ghost_name
                    );
                    return Ok(QueryResult::GroupedAggregation(rows));
                }
            }
        }
        tracing::info!(
            "Router: GROUP BY routed to ghost PreComputed '{}'",
            ghost_name
        );
        match engine
            .ghost_manager
            .read_precomputed(ghost_name, &group_fields, &router_filters)
        {
            Ok(crate::ghost::GhostAggregates::Grouped(group_map)) => {
                // Shared row builder — byte-identical to the O(N) metric-order
                // read above, so a TOP served from the order equals this path.
                let rows: Vec<std::collections::BTreeMap<String, Value>> = group_map
                    .into_iter()
                    .map(|(gk, state)| {
                        crate::ghost::metric_order::group_state_to_row(&group_fields, &gk, &state)
                    })
                    .collect();
                return Ok(QueryResult::GroupedAggregation(rows));
            }
            Ok(_) => {} // fall through
            Err(XyzError::GhostNotFound(name)) => {
                // TOCTOU fallback (Finding 1). See execute_scan_aggregate.
                tracing::debug!(
                    "GROUP BY: PreComputed ghost '{}' not found mid-read; falling back to Primary",
                    name
                );
                fallback_unregister_ghost(engine, lobe_id, &name);
            }
            Err(other) => return Err(other),
        }
    }

    let mut groups: std::collections::BTreeMap<
        String,
        (std::collections::BTreeMap<String, Value>, AggAccumulator),
    > = std::collections::BTreeMap::new();
    let limit = stmt.limit;

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);

    let mut observe_record = |record: &Record| {
        let key_parts: Vec<String> = group_fields
            .iter()
            .map(|f| canonical_key(resolve_path(&record.fields, f)))
            .collect();
        let key = key_parts.join("|");

        let (_, acc) = groups.entry(key).or_insert_with(|| {
            let kv: std::collections::BTreeMap<String, Value> = group_fields
                .iter()
                .filter_map(|f| resolve_path(&record.fields, f).map(|v| (f.clone(), v.clone())))
                .collect();
            (kv, AggAccumulator::new(funcs.clone()))
        });
        acc.observe(record);
    };

    match scan_source {
        ScanSource::Primary | ScanSource::GhostPreComputed(_) | ScanSource::Ghost(_) => {
            let mut counted = 0u64;
            // Finding 13: gravity-indexed fast path for SCAN | GROUP BY |
            // AGGREGATE that doesn't hit a PreComputed ghost (e.g. when a
            // matching ghost is missing or the group_fields don't match a
            // ghost's group_fields). The bounded range scan still beats the
            // full-lobe scan; aggregation runs over fewer records.
            if let Some(ghash) = detect_gravity_eq(engine, &stmt.lobe, &core_filters) {
                let (key_min, key_max) = SpatialKey::prefix_for_gravity(lobe_id, ghash);
                for entry in engine
                    .turba
                    .spatial
                    .range_stream(key_min.as_slice(), key_max.as_slice())
                    .map_err(|e| XyzError::Storage(e.to_string()))?
                {
                    let val = &entry.value;
                    if let Ok(record) =
                        crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
                        && record.matches_filters(&core_filters)
                    {
                        observe_record(&record);
                        counted += 1;
                        if let Some(lim) = limit
                            && counted >= lim
                        {
                            break;
                        }
                    }
                }
            } else {
                let prefix = lobe_id.to_be_bytes();
                for entry in engine
                    .spatial_tree()
                    .prefix_iter(&prefix)
                    .map_err(|e| XyzError::Storage(e.to_string()))?
                {
                    let val = &entry.value;
                    if let Ok(record) =
                        crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
                        && record.matches_filters(&core_filters)
                    {
                        observe_record(&record);
                        counted += 1;
                        if let Some(lim) = limit
                            && counted >= lim
                        {
                            break;
                        }
                    }
                }
            }
        }
    }

    let results: Vec<std::collections::BTreeMap<String, Value>> = groups
        .into_values()
        .map(|(key_vals, acc)| {
            let mut row = key_vals;
            if let QueryResult::Aggregation(agg) = acc.finalize() {
                row.extend(agg);
            }
            row
        })
        .collect();

    Ok(QueryResult::GroupedAggregation(results))
}

// ─── Streaming scan (writes records directly to a sync writer) ──────────

/// Write a single chunk: [length: u32 BE][payload]
fn write_chunk<W: std::io::Write>(writer: &mut W, payload: &[u8]) -> std::io::Result<()> {
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    Ok(())
}

/// Execute a SCAN and write each matching record as a chunk to the writer.
/// Used for TCP streaming — avoids materializing Vec<Record> in RAM.
/// Only works for SCAN without ORDER BY (ORDER BY requires heap).
/// Returns the count of records written.
pub fn execute_scan_streaming<W: std::io::Write>(
    engine: &Engine,
    stmt: &ScanStmt,
    writer: &mut W,
    serialize_fn: fn(&Record) -> Vec<u8>,
) -> Result<u64> {
    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(&stmt.lobe)
        .ok_or_else(|| XyzError::LobeNotFound(stmt.lobe.clone()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);

    let flat_filters = crate::ops::filter_expr_to_flat(&stmt.filter_expr);
    let core_filters = convert_filters(&flat_filters);
    let router_filters: Vec<_> = core_filters
        .iter()
        .map(|(f, o, v)| (f.clone(), *o, v.clone()))
        .collect();

    // C-16 / Entry 22: anchor-Eq override before ghost routing.
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let anchor_guard = engine.anchor_registry.read();
    let anchored = anchor_guard.get_anchors(&lobe_name);
    let routers = engine.ghost_routers.read();
    let scan_source = match routers.get(&lobe_id) {
        Some(router) if router.has_ghosts() => router.plan_scan(
            &router_filters,
            true,
            None,
            false,
            &[],
            false,
            stmt.filter_expr.as_ref(),
            Some(anchored),
        ),
        _ => ScanSource::Primary,
    };
    drop(routers);
    drop(anchor_guard);

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let scan_start = Instant::now();
    let mut count = 0u64;
    let limit = stmt.limit;

    match scan_source {
        ScanSource::Primary | ScanSource::Ghost(_) | ScanSource::GhostPreComputed(_) => {
            let prefix = lobe_id.to_be_bytes();
            for entry in engine
                .spatial_tree()
                .prefix_iter(&prefix)
                .map_err(|e| XyzError::Storage(e.to_string()))?
            {
                let val = &entry.value;
                if let Ok(record) =
                    crate::ops::deserialize_hydrated(engine, &entry.key, val, &lobe_name, fd)
                    && record.matches_filters(&core_filters)
                {
                    let chunk = serialize_fn(&record);
                    if write_chunk(writer, &chunk).is_err() {
                        break; // Client disconnected
                    }
                    count += 1;
                    if let Some(lim) = limit
                        && count >= lim
                    {
                        break;
                    }
                }
            }
        }
    }

    // Record telemetry (simple record, no auto-ghost for streaming)
    let mut telemetry = engine.scan_telemetry.write();
    telemetry.record(ScanTelemetry {
        lobe: stmt.lobe.clone(),
        filter_desc: format!("{:?}", &stmt.filter_expr),
        source: match &scan_source {
            ScanSource::Primary => "primary".to_string(),
            ScanSource::Ghost(name) => format!("ghost:{name}"),
            ScanSource::GhostPreComputed(name) => format!("ghost_pre:{name}"),
        },
        records_scanned: 0,
        records_returned: count,
        duration: scan_start.elapsed(),
    });

    Ok(count)
}

// ─── Value comparison for ORDER BY ─────────────────────────────────────────

fn compare_values(a: &Option<Value>, b: &Option<Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(va), Some(vb)) => compare_value_inner(va, vb),
    }
}

fn compare_value_inner(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        // NULLS LAST: Null sorts after everything
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Int(a), Value::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float(a), Value::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// v0.2.5.1 — CURSOR pagination (plain SCAN only; ORDER BY + GHOST = v0.3)
// ════════════════════════════════════════════════════════════════════════════

/// Output of a paginated primary SCAN. `page_tail_key` is the SpatialKey
/// of the last record in `records` (truncated to the page boundary), used
/// to seed the next cursor when `has_more` is true.
struct PaginatedScanOutput {
    records: Vec<Record>,
    page_tail_key: Option<[u8; xyzdb_core::key::SPATIAL_KEY_SIZE]>,
    has_more: bool,
}

/// Bounded primary SCAN with optional cursor seek + overscan-by-one for
/// `has_more` detection. Always traverses the spatial keyspace (no ghost
/// routing): cursor + ghost paging is v0.3 scope.
fn scan_primary_paginated(
    engine: &Engine,
    lobe_id: u16,
    filter_expr: &Option<xytalk_parser::ast::FilterExpr>,
    effective_limit: u64,
    start_after: Option<&[u8]>,
) -> Result<PaginatedScanOutput> {
    use xyzdb_core::key::SPATIAL_KEY_SIZE;

    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let mut records: Vec<Record> = Vec::new();
    let mut page_tail_key: Option<[u8; SPATIAL_KEY_SIZE]> = None;
    let mut has_more = false;
    let target = effective_limit.saturating_add(1);

    // Helper: process one entry, returning `true` to break (page full).
    let consume = |entry_key: &[u8],
                   entry_value: &[u8],
                   records: &mut Vec<Record>,
                   page_tail_key: &mut Option<[u8; SPATIAL_KEY_SIZE]>,
                   has_more: &mut bool|
     -> bool {
        let Ok(record) =
            crate::ops::deserialize_hydrated(engine, entry_key, entry_value, &lobe_name, fd)
        else {
            return false;
        };
        if !crate::ops::record_matches_opt_expr(&record, filter_expr) {
            return false;
        }
        records.push(record);
        let len = records.len() as u64;
        if len <= effective_limit && entry_key.len() == SPATIAL_KEY_SIZE {
            let mut k = [0u8; SPATIAL_KEY_SIZE];
            k.copy_from_slice(entry_key);
            *page_tail_key = Some(k);
        }
        if len >= target {
            // Overscan trigger: drop the extra record, mark has_more, stop.
            records.truncate(effective_limit as usize);
            *has_more = true;
            return true;
        }
        false
    };

    if let Some(cursor_key) = start_after {
        // Open lower bound: append 0x00 to skip strictly past the cursor key.
        // Upper bound: next-lobe prefix. lobe_id == u16::MAX is unreachable
        // under the LobeRegistry's u16 namespace cap, so checked_add is fine.
        let mut start = Vec::with_capacity(cursor_key.len() + 1);
        start.extend_from_slice(cursor_key);
        start.push(0x00);
        let next_lobe_bytes = lobe_id
            .checked_add(1)
            .map(u16::to_be_bytes)
            .ok_or_else(|| XyzError::Internal("lobe_id overflow on cursor end-bound".into()))?;
        let entries = engine
            .turba
            .spatial
            .range_stream(&start, &next_lobe_bytes)
            .map_err(|e| XyzError::Storage(e.to_string()))?;
        for entry in entries {
            if consume(
                &entry.key,
                &entry.value,
                &mut records,
                &mut page_tail_key,
                &mut has_more,
            ) {
                break;
            }
        }
    } else {
        let prefix = lobe_id.to_be_bytes();
        let entries = engine
            .turba
            .spatial
            .prefix_iter(&prefix)
            .map_err(|e| XyzError::Storage(e.to_string()))?;
        for entry in entries {
            if consume(
                &entry.key,
                &entry.value,
                &mut records,
                &mut page_tail_key,
                &mut has_more,
            ) {
                break;
            }
        }
    }

    Ok(PaginatedScanOutput {
        records,
        page_tail_key,
        has_more,
    })
}

/// Cursor-driven SCAN: decode the opaque token, validate against the
/// current statement, force `ScanSource::Primary`, run a bounded range
/// scan, and emit the next-page cursor when more records remain.
fn execute_cursor_scan(
    engine: &Engine,
    stmt: ScanStmt,
    token: &str,
    scan_start: Instant,
) -> Result<ScanResult> {
    use crate::cursor::{
        CURSOR_FORMAT_V2, CursorPayload, decode_cursor, encode_cursor, filter_checksum,
    };

    // Decode the token (validates format_ver internally).
    let payload = decode_cursor(token)?;

    // Resolve the lobe and verify it matches the cursor's binding.
    let lobes = engine.lobe_registry.read();
    let lobe_config = lobes
        .get(&stmt.lobe)
        .ok_or_else(|| XyzError::LobeNotFound(stmt.lobe.clone()))?;
    let lobe_id = lobe_config.id;
    drop(lobes);
    if payload.lobe_id != lobe_id {
        return Err(XyzError::InvalidQuery(format!(
            "cursor invalid: token issued for lobe_id={}, current request targets lobe_id={lobe_id}",
            payload.lobe_id
        )));
    }

    // Verify filter checksum: cursors are bound to the WHERE clause that
    // produced them. Re-using a cursor under a different filter would
    // silently produce an inconsistent page.
    let current_checksum = filter_checksum(&stmt.filter_expr);
    if current_checksum != payload.filter_checksum {
        return Err(XyzError::InvalidQuery(
            "cursor invalid: WHERE clause does not match the cursor's binding; \
             cursors are only valid for the exact filter that produced them"
                .into(),
        ));
    }

    let effective_limit = stmt.limit.unwrap_or(SCAN_LIMIT_DEFAULT);

    let out = scan_primary_paginated(
        engine,
        lobe_id,
        &stmt.filter_expr,
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

    let filter_desc = format!("{:?}", &stmt.filter_expr);
    let duration = scan_start.elapsed();
    let telemetry = ScanTelemetry {
        lobe: stmt.lobe.clone(),
        filter_desc,
        source: "primary_cursor".to_string(),
        records_scanned: 0,
        records_returned: out.records.len() as u64,
        duration,
    };
    // Cursor pages are not auto-ghost candidates: each page is a distinct
    // scan shape (different start_after) and we don't want to thrash
    // telemetry. Record under `record_routed` so SHOW SCAN STATS still
    // surfaces the activity without feeding the auto-promotion engine.
    engine.scan_telemetry.write().record_routed(telemetry);

    Ok(ScanResult {
        query_result: QueryResult::PaginatedRecords {
            records: out.records,
            cursor: new_cursor,
            has_more: out.has_more,
            budget_stop: None, // cursor-resumed SCAN page, not a NEAREST budget stop
        },
        auto_ghost: None,
    })
}

#[cfg(test)]
mod truncation_tests {
    use super::*;
    use crate::engine::Engine;

    /// The `scan_primary_full_expr` leaf (the non-gravity fallback route, reached
    /// when a routed ghost is evicted mid-scan or a PreComputed ghost surfaces
    /// unexpectedly) must overscan and report truncation, so its callers can
    /// signal `has_more`. Tested directly because those fallbacks are not
    /// deterministically triggerable end to end.
    #[test]
    fn full_expr_leaf_overscans_and_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "l""#).unwrap();
        for i in 0..1001 {
            engine
                .run(&format!(r#"PUT {{id:"r{i}", body:"m{i}"}} IN "l""#))
                .unwrap();
        }
        let lobe_id = engine.lobe_registry.read().get("l").unwrap().id;

        // Past the cap → truncated to the cap, flagged truncated.
        let (recs, more) = scan_primary_full_expr(&engine, lobe_id, &None, Some(1000)).unwrap();
        assert_eq!(recs.len(), 1000);
        assert!(more, "full-expr leaf must report truncation past the cap");

        // Uncapped (NEAREST-style) → everything, never flagged.
        let (all, more_none) = scan_primary_full_expr(&engine, lobe_id, &None, None).unwrap();
        assert_eq!(all.len(), 1001);
        assert!(!more_none, "an uncapped scan never reports truncation");

        // Exactly filling the cap with nothing beyond → no false positive.
        let (exact, more_exact) =
            scan_primary_full_expr(&engine, lobe_id, &None, Some(1001)).unwrap();
        assert_eq!(exact.len(), 1001);
        assert!(
            !more_exact,
            "a result that exactly fills the cap must not over-signal"
        );
    }
}
