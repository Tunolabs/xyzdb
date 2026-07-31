//! `NEAREST` pipeline step — semantic top-k over the records produced by the
//! preceding (gravity-bounded) scan.
//!
//! This is the contextual-RAG primitive: the prior `SCAN`/`FIND` already bounds
//! the candidate set to one gravity bucket (a document / tenant / conversation),
//! so ranking by embedding similarity is an **exact** brute-force pass over a
//! small set — no ANN index, no recall trade-off. Cross-bucket / global search
//! is a separate mechanism.

use crate::engine::Engine;
use crate::ops::literal_to_value;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use xytalk_parser::ast::{NearestQuery, NearestStmt, ScanStmt};
use xyzdb_core::distance::{self, Metric};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::SpatialKey;
use xyzdb_core::record::{Record, deserialize_record, read_vector_prefix_raw_norm};
use xyzdb_core::value::Value;

/// Dims scored between Cauchy–Schwarz bound checks in [`distance::cosine_pruned`].
/// Amortizes the per-check `sqrt`s; 32 = one check every 1/8 of a 256-d vector.
const PRUNE_BLOCK: usize = 32;

/// Candidates scanned between `--nearest-budget-ms` clock checks (M2.2 airbag).
/// One `Instant::elapsed` per this many entries keeps the check overhead well
/// under 1% while bounding the worst-case overrun to ~one stride of scoring.
/// Shared with the unfused gravity scan in `ops::scan` (M2.1) so both the fused
/// and the decoupled-from-cap paths trip the budget on the same cadence.
pub(crate) const BUDGET_CHECK_STRIDE: usize = 1024;

/// Rank `records` by similarity of their `stmt.field` embedding to the query
/// vector and return the top `stmt.k`, most similar first.
///
/// Records missing the field, whose field is not a numeric vector, or whose
/// dimension differs from the query are skipped, so a heterogeneous bucket
/// stays usable.
///
/// # Arguments
/// * `records` - candidates from the preceding pipeline step.
/// * `stmt` - the parsed `NEAREST(field, query, k, metric)`.
///
/// # Returns
/// The top-`k` records ordered by descending similarity.
///
/// # Errors
/// Returns [`XyzError::InvalidQuery`] if the metric is unknown or the query is
/// not a list of numbers.
pub fn execute_nearest(records: Vec<Record>, stmt: &NearestStmt) -> Result<Vec<Record>> {
    let metric = Metric::parse(&stmt.metric).ok_or_else(|| {
        XyzError::InvalidQuery(format!(
            "NEAREST: unknown metric '{}'. Expected cosine, dot, or l2",
            stmt.metric
        ))
    })?;
    // Resolve the query vector. For REF, also note which record to exclude
    // (a "more like this" must not return the reference itself).
    let (query, exclude): (Vec<f32>, Option<String>) = match &stmt.query {
        NearestQuery::Vector(lit) => (
            distance::as_vector(&literal_to_value(lit)).ok_or_else(|| {
                XyzError::InvalidQuery("NEAREST: query must be a list of numbers".into())
            })?,
            None,
        ),
        NearestQuery::Param(name) => {
            // Params are substituted to a vector before execution; an unbound
            // one reaching here means the query was run without binding it.
            return Err(XyzError::InvalidQuery(format!(
                "NEAREST: unbound parameter ${name} — pass it via run_with_params"
            )));
        }
        NearestQuery::Ref(id) => (resolve_ref(&records, &stmt.field, id)?, Some(id.clone())),
    };

    let k = stmt.k as usize;
    if k == 0 {
        return Ok(Vec::new());
    }

    // Bounded min-heap of size k: keep the k highest similarities. `Scored`
    // orders so the *lowest* score sits at the heap top, so once the heap holds
    // k+1 we pop the worst and stay at k.
    let mut heap: BinaryHeap<Scored> = BinaryHeap::with_capacity(k + 1);
    for rec in records {
        if let Some(ex) = &exclude
            && has_field_value(&rec, ex)
        {
            continue; // the REF record itself
        }
        let Some(v) = rec.fields.get(&stmt.field).and_then(distance::as_vector) else {
            continue; // no embedding, or not a numeric vector
        };
        // The candidate is a deserialized Vec<f32>; view it as packed bytes (the
        // form the scorer consumes) — zero-copy, the Vec is 4-byte aligned.
        let Some(score) = distance::similarity(metric, &query, bytemuck::cast_slice(&v)) else {
            continue; // dimension mismatch / undefined
        };
        heap.push(Scored { score, rec });
        if heap.len() > k {
            heap.pop();
        }
    }

    let mut scored = heap.into_vec();
    // Most similar first; ties broken by ascending lid (deterministic — see
    // `Scored`'s Ord). The fused fast path mirrors this exact ordering.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(a.rec.lid.cmp(&b.rec.lid))
    });
    Ok(scored.into_iter().map(|s| s.rec).collect())
}

/// True if any field of `rec` holds `Value::Text(id)` — used to locate and
/// exclude the `REF` record by a unique id value.
fn has_field_value(rec: &Record, id: &str) -> bool {
    rec.fields
        .values()
        .any(|v| matches!(v, Value::Text(t) if t == id))
}

/// Resolve a `REF "id"` query: find the unique scanned record whose field value
/// equals `id` and return its `field` embedding as the query vector.
///
/// # Errors
/// [`XyzError::InvalidQuery`] if no record matches, more than one matches, or
/// the matched record has no numeric `field`.
fn resolve_ref(records: &[Record], field: &str, id: &str) -> Result<Vec<f32>> {
    let mut found: Option<&Record> = None;
    for rec in records {
        if has_field_value(rec, id) {
            if found.is_some() {
                return Err(XyzError::InvalidQuery(format!(
                    "NEAREST: REF \"{id}\" is ambiguous — matches more than one scanned record"
                )));
            }
            found = Some(rec);
        }
    }
    let rec = found.ok_or_else(|| {
        XyzError::InvalidQuery(format!(
            "NEAREST: REF \"{id}\" not found among the scanned records"
        ))
    })?;
    rec.fields
        .get(field)
        .and_then(distance::as_vector)
        .ok_or_else(|| {
            XyzError::InvalidQuery(format!("NEAREST: REF \"{id}\" has no numeric '{field}'"))
        })
}

/// A record paired with its similarity score. Ordered so that a [`BinaryHeap`]
/// (a max-heap) keeps the **lowest** score at the top — turning it into a
/// bounded top-k min-heap. `NaN` scores compare as lowest and are dropped first.
struct Scored {
    score: f64,
    rec: Record,
}

impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Scored {}
impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: a smaller score is "greater" so the max-heap surfaces it for
        // pop(). NaN (partial_cmp == None) is treated as the smallest score.
        // Tie on score → the HIGHER lid is "greater" (popped/dropped first), so
        // the retained top-k and the final order are deterministic: (score
        // descending, lid ascending). This determinism is load-bearing — the V3
        // prefix fast path must produce a bit-identical top-k to this full path.
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(Ordering::Equal)
            .then(self.rec.lid.cmp(&other.rec.lid))
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Fused [Scan, Nearest] fast path (V5 column-primary, V3/V4 prefix fallback)
// ════════════════════════════════════════════════════════════════════════════

/// Execute a fused `SCAN ... | NEAREST(field, q, k, metric)` pipeline.
///
/// When the lobe declares the queried field as its searchable vector, the
/// NEAREST query is a literal vector, and the SCAN pins exactly one gravity
/// bucket with no residual per-record filter, this reads only the V5 vector
/// column of each record in the bucket (falling back to the inline V3/V4
/// vector prefix for un-migrated buckets), scores it, keeps the top-k, and
/// fully deserializes ONLY the surviving top-k. The result is bit-identical to the
/// unfused path (`scan → execute_nearest`).
///
/// Any case outside that window falls back to the exact existing path: a full
/// `execute_scan` followed by `execute_nearest`, so behaviour is unchanged.
///
/// # Arguments
/// * `engine` - the engine (lobe/field registries, spatial keyspace).
/// * `scan` - the preceding SCAN statement.
/// * `nearest` - the NEAREST statement.
///
/// # Returns
/// `(records, truncated)`: the top-`k` records ordered (score DESC, lid ASC),
/// identical to the unfused path, plus a `truncated` flag. `truncated` is `true`
/// only when the fused fast path's score-ordered hydration was cut by the budget
/// — the records are then a prefix-correct partial (the highest-scoring passers
/// found within budget). The fallback path is exact and always returns `false`.
///
/// # Errors
/// Propagates query/storage errors from the underlying scan, deserialization,
/// or NEAREST execution, and `NearestBudgetExceeded` if the budget is hit during
/// the (unbounded) scoring scan — but NOT during the bounded hydration tail,
/// which degrades to a truncated partial instead.
pub fn execute_scan_nearest(
    engine: &Engine,
    scan: ScanStmt,
    nearest: &NearestStmt,
) -> Result<(Vec<Record>, Option<xyzdb_core::result::BudgetStop>)> {
    if let Some((records, budget_stop)) = try_prefix_scan_nearest(engine, &scan, nearest)? {
        return Ok((records, budget_stop));
    }
    // Fallback (M2.1): the full path, but the feeding SCAN is decoupled from the
    // default cap so an exact top-k sees the WHOLE gravity bucket (no recall
    // cliff at SCAN_LIMIT_DEFAULT). The uncap is bounded by the NEAREST budget
    // airbag, checked DURING the gravity-bucket scan — not after it materializes.
    // An explicit `LIMIT` on the SCAN is still honoured; only the implicit cap is
    // lifted, and only on the gravity-indexed path (residual/full-lobe scans stay
    // capped pending M2.3 hydrate-until-k).
    let scan_result =
        crate::ops::scan::execute_scan_for_nearest(engine, scan, engine.nearest_budget_ms)?;
    let records = match scan_result.query_result {
        crate::engine::QueryResult::Records(r) => r,
        crate::engine::QueryResult::PaginatedRecords { records, .. } => records,
        other => {
            return Err(XyzError::InvalidQuery(format!(
                "NEAREST requires records from the preceding SCAN, got: {other:?}"
            )));
        }
    };
    // The fallback filters DURING the scan (not hydrate-until-k), so it never
    // hits the residual-selectivity trap and is always a complete answer.
    let records = execute_nearest(
        records,
        &NearestStmt {
            field: nearest.field.clone(),
            query: nearest.query.clone(),
            k: nearest.k,
            metric: nearest.metric.clone(),
        },
    )?;
    // The fallback is an exact, complete answer — it never truncates, so no
    // budget_stop signal.
    Ok((records, None))
}

/// Attempt the fused fast path (V5 column-primary, V3/V4 prefix fallback).
/// Returns `Ok(Some((records, truncated)))` when every applicability rule holds
/// and the bucket has no hash-collision intruder; `Ok(None)` to signal the
/// caller must fall back to the full path.
///
/// `truncated` is `true` only when the hydration budget cut the score-ordered
/// hydration short (M2.3): the records are then a PREFIX-CORRECT partial — the
/// highest-scoring passers found so far — not a complete answer. It is `false`
/// both for a full top-k and for a complete-but-short answer (fewer than k rows
/// pass the residual and the whole bucket was hydrated within budget).
fn try_prefix_scan_nearest(
    engine: &Engine,
    scan: &ScanStmt,
    nearest: &NearestStmt,
) -> Result<Option<(Vec<Record>, Option<xyzdb_core::result::BudgetStop>)>> {
    // Rule: lobe declares a searchable vector AND nearest.field == that field.
    let Some(spec) = engine.get_vector_spec(&scan.lobe) else {
        return Ok(None);
    };
    if spec.field != nearest.field {
        return Ok(None);
    }

    // Rule: literal Vector query only (REF/Param fall back).
    let NearestQuery::Vector(lit) = &nearest.query else {
        return Ok(None);
    };
    let Some(query) = distance::as_vector(&literal_to_value(lit)) else {
        // Not a numeric vector — let the full path raise the same error.
        return Ok(None);
    };

    // Rule: SCAN ordering / pagination disqualify the simple bucket sweep.
    if scan.order_by.is_some() || scan.cursor.is_some() {
        return Ok(None);
    }

    let metric = match Metric::parse(&nearest.metric) {
        Some(m) => m,
        None => return Ok(None), // unknown metric — full path raises the error.
    };

    // Rule: the SCAN selects the bucket purely by the gravity predicate, with
    // NO residual per-record filter. Detect via the same gravity-eq mechanism
    // the codebase uses, then confirm every flat-AND condition is on a gravity
    // field (anything else is a residual the prefix path can't evaluate).
    let core_filters = crate::ops::convert_filter_expr(&scan.filter_expr);
    let Some(gravity_hash) = crate::ops::scan::detect_gravity_eq(engine, &scan.lobe, &core_filters)
    else {
        return Ok(None);
    };
    if scan.filter_expr.is_some() && core_filters.is_empty() {
        // OR/NOT expression: filter_expr_to_flat collapsed to empty. Residual.
        return Ok(None);
    }
    let Some(gravity_spec) = engine.get_gravity_spec(&scan.lobe) else {
        return Ok(None);
    };
    let gravity_fields = gravity_spec.fields();
    // M2.3: a predicate on a non-gravity field is a residual per-record filter.
    // Rather than bail to the full path (which deserializes the WHOLE bucket), the
    // fused path scores every candidate via the cheap prefix, then hydrates in
    // score order and applies the filter until k records PASS (hydrate-until-k) —
    // exact, and only as many full deserializes as needed to find the k winners.
    // Only the flat-AND residual is handled; the OR/NOT case (above) still falls
    // back, and a pure-legacy (non-V5) bucket with a residual falls back too.
    let has_residual = core_filters
        .iter()
        .any(|(field, _, _)| !gravity_fields.contains(&field.as_str()));

    let k = nearest.k as usize;
    if k == 0 {
        return Ok(Some((Vec::new(), None)));
    }

    // Resolve the query field's id once, so per-record prefixes can be matched
    // by id without consulting the dict in the hot loop.
    let lobe_id = {
        let lobes = engine.lobe_registry.read();
        let cfg = lobes
            .get(&scan.lobe)
            .ok_or_else(|| XyzError::LobeNotFound(scan.lobe.clone()))?;
        cfg.id
    };
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr_guard = engine.field_registry.read();
    let fd = fr_guard.get_dict(lobe_id);
    let query_field_id: Option<u16> = fd.and_then(|d| {
        d.to_names()
            .iter()
            .position(|n| n == &nearest.field)
            .map(|p| p as u16)
    });

    // Sub-gravity: when the query ALSO pins the lobe's satellite field, narrow
    // the scoring scan to that satellite sub-range. This is the missing half of
    // the sub-gravity win for NEAREST: with an Eq on the satellite field the
    // candidate set IS the satellite, so scoring within it is the exact top-k of
    // the filtered set (not an approximation) — the whole-bucket score+hydrate
    // that this query used to pay collapses to the satellite. The residual below
    // still drops hash16 collisions.
    let sat = engine.detect_satellite_eq(&scan.lobe, &core_filters);
    let (key_min, key_max) = match sat {
        Some(s) => SpatialKey::prefix_for_satellite(lobe_id, gravity_hash, s),
        None => SpatialKey::prefix_for_gravity(lobe_id, gravity_hash),
    };
    // On the satellite path the fused residual is the anti-collision guard; on in
    // production, droppable only by the SAT_SKIP_ANTICOLLISION_RESIDUAL knob (the
    // negative control). The plain gravity path keeps the residual unconditional.
    let apply_residual = sat.is_none() || engine.satellite_residual_active();

    // Cosine pruning precompute (lever C): ‖query‖ and its suffix sum-of-squares,
    // built once. Used only for the cosine metric — the Cauchy–Schwarz bound is
    // cosine-specific; dot/L2 keep the plain scorer.
    // ‖query‖ via the canonical reduction so cosine_pruned's denominator
    // (na · √nb2) is bit-identical to similarity(Cosine, ·)'s norm(a) · norm(b).
    // A naïve Σx² fold here would diverge from the SIMD path and break the
    // survivor bit-identity the fused path contracts for.
    let (query_na, query_suffix) = if metric == Metric::Cosine {
        (distance::norm(&query), distance::suffix_norm2(&query))
    } else {
        (0.0, Vec::new())
    };

    // Bounded top-k min-heap of CHEAP candidates: score + lid + the spatial key
    // + the column bytes (for V5 survivors). Scoring reads ONLY the ~1 KB vector
    // column per record — never the ~4 KB blob — and a full `deserialize_record`
    // runs only for the surviving top-k afterwards. This is the RAM win: ranking
    // touches the column keyspace, not the record blobs.
    let mut heap: BinaryHeap<PrefixCand> = BinaryHeap::with_capacity(k + 1);

    // M2.2 airbag: bound the bucket scan by wall-clock so a pathologically large
    // bucket aborts with a clear error instead of hanging. `0` disables. Shared
    // across the V5 and V3/V4 loops (only one runs). `started` is consulted only
    // when the budget is enabled, every `BUDGET_CHECK_STRIDE` candidates.
    let budget_ms = engine.nearest_budget_ms;
    let started = std::time::Instant::now();
    let mut scanned = 0usize;

    // Primary path: range the `vectors` column over the SAME gravity-bucket key
    // range as the records (the column is keyed by the spatial key). Each value
    // is the V4-shaped mini-blob that `read_vector_prefix_raw_norm` parses, so
    // scoring/pruning are byte-identical to the inline-prefix path — only the
    // source of the f32 bytes moved out of the blob. The scorer reads `fbytes`
    // directly (unaligned f32x8 loads) — no decode into an aligned buffer.
    let mut saw_column = false;
    // Stream the bucket block-by-block (NOT `range`, which collects the whole
    // bucket into a Vec — the query balloon that OOM'd 100k+). The bounded top-k
    // heap already caps retained state at k; streaming caps the SOURCE working set
    // at O(block), decoupling scan RAM from bucket size N. `range_stream` is the
    // INCLUSIVE `[key_min, key_max]` form (a half-open bound would drop an entry on
    // the saturated key_max tail); the top-k it feeds is byte-identical to `range`.
    for entry in engine
        .turba
        .vectors
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
        let column = &entry.value;
        let Some((lid, field_id, fbytes, norm_sq)) = read_vector_prefix_raw_norm(column) else {
            continue; // not a vector column entry — skip (defensive).
        };
        saw_column = true;
        if Some(field_id) != query_field_id {
            continue; // a different field's column — not this NEAREST's target.
        }
        // For cosine, `cosine_pruned` skips the per-candidate norm pass (norm_sq
        // is always present in the V4-shaped column) and Cauchy–Schwarz-aborts a
        // candidate that provably cannot beat the current k-th best — both
        // bit-exact: a survivor scores identically to the full path, an abort is
        // strictly worse so it would never have made the top-k. dot/L2 use the
        // plain scorer.
        let scored = if metric == Metric::Cosine {
            // No residual: prune against the current k-th best score. With a
            // residual we must NOT prune by score — a top-scored candidate may
            // FAIL the filter while a lower-scored one passes, so a score-abort
            // could drop a real winner. Every candidate is fully scored and kept.
            let thr = if has_residual {
                None
            } else {
                (heap.len() == k)
                    .then(|| heap.peek().map(|c| c.score))
                    .flatten()
            };
            distance::cosine_pruned(
                &query,
                query_na,
                &query_suffix,
                fbytes,
                norm_sq,
                thr,
                PRUNE_BLOCK,
            )
        } else {
            distance::similarity(metric, &query, fbytes)
        };
        let Some(score) = scored else {
            continue; // dim mismatch / undefined / pruned — skip, like the full path.
        };
        heap.push(PrefixCand {
            score,
            lid,
            key: entry.key.to_vec(),
            // No residual: keep the column to hydrate the k survivors with no
            // re-fetch. With a residual we retain EVERY candidate, so storing all
            // columns would rebuild the query balloon — keep None and re-fetch the
            // column only for the k that pass the filter.
            column: if has_residual {
                None
            } else {
                Some(column.to_vec())
            },
        });
        // With a residual the heap holds the whole bucket (we don't yet know which
        // top-scored candidates survive the filter); truncation happens after the
        // filter, in the hydrate-until-k tail.
        if !has_residual && heap.len() > k {
            heap.pop();
        }
    }

    // V3/V4 fallback: a legacy / un-migrated bucket has NO column entries, but
    // its records still carry the vector inline in the blob. Scan the blob
    // prefix exactly as before so old data keeps working. (Fresh-seeded data is
    // all-V5, so this path is dormant in tests.)
    if !saw_column {
        // Hydrate-until-k (M2.3) is built on the V5 column layout. A pure-legacy
        // V3/V4 bucket carrying a residual is rare; hand it to the exact full path
        // rather than special-casing the inline-blob layout here.
        if has_residual {
            return Ok(None);
        }
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
            let blob = &entry.value;
            let (lid, score) = match read_vector_prefix_raw_norm(blob) {
                Some((lid, field_id, fbytes, norm_sq)) if Some(field_id) == query_field_id => {
                    let scored = if metric == Metric::Cosine {
                        let thr = (heap.len() == k)
                            .then(|| heap.peek().map(|c| c.score))
                            .flatten();
                        distance::cosine_pruned(
                            &query,
                            query_na,
                            &query_suffix,
                            fbytes,
                            norm_sq,
                            thr,
                            PRUNE_BLOCK,
                        )
                    } else {
                        distance::similarity(metric, &query, fbytes)
                    };
                    let Some(score) = scored else {
                        continue;
                    };
                    (lid, score)
                }
                _ => {
                    let Ok(record) = deserialize_record(blob, &lobe_name, fd) else {
                        continue;
                    };
                    // Hash-collision guard: a record from a foreign gravity value
                    // can only share this key range via a 48-bit collision. The
                    // full path drops it post-range; to stay bit-identical we bail
                    // to the full path entirely if we ever observe one.
                    if !crate::ops::record_matches_opt_expr(&record, &scan.filter_expr) {
                        return Ok(None);
                    }
                    let Some(v) = record
                        .fields
                        .get(&nearest.field)
                        .and_then(distance::as_vector)
                    else {
                        continue;
                    };
                    let Some(score) =
                        distance::similarity(metric, &query, bytemuck::cast_slice(&v))
                    else {
                        continue;
                    };
                    (record.lid, score)
                }
            };
            heap.push(PrefixCand {
                score,
                lid,
                key: entry.key.to_vec(),
                column: None,
            });
            if heap.len() > k {
                heap.pop();
            }
        }
    }

    // Final order: (score DESC, lid ASC) — byte-for-byte the `execute_nearest`
    // contract. Materialize ONLY the survivors, by re-fetching their k blobs by
    // spatial key (k point-gets) — the heap held the 22-byte key, not a ~4 KB
    // blob clone per scanned record (that clone, ~N×4 KB, was the whole reason
    // an earlier cut measured SLOWER than the full path).
    let mut cands = heap.into_vec();
    cands.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(a.lid.cmp(&b.lid))
    });
    // No residual: `cands` IS the top-k — hydrate all of them. With a residual:
    // `cands` is the whole bucket in score order; hydrate best-first, apply the
    // FULL filter, and keep passers until k (hydrate-until-k). Either way,
    // materialize by re-fetching each blob by spatial key (a point-get) — the heap
    // held the 22-byte key, never a per-record blob clone.
    // STEP 1 of the A/B switch — accumulator only, no behaviour change.
    //
    // Was a prefix `Vec<Record>`: correct ONLY because `cands` is score-ordered, so
    // "the first k pushed" happened to equal "the top k". That coupling is what
    // blocks a key-ordered pass (B), where a later candidate can outrank an earlier
    // one. So the accumulator becomes a bounded top-k heap and the OUTPUT ORDER is
    // re-established explicitly at the end.
    //
    // It reuses `Scored` — the very struct and ordering the unfused full path uses —
    // and the final sort below is character-for-character the full path's. That is
    // deliberate: the fused path contracts to produce a BIT-IDENTICAL top-k to the
    // full path, and the cheapest way to keep that true is to share the machinery
    // rather than to re-derive an equivalent one. A heap pops in the opposite order,
    // so without that explicit final sort the tie-break silently inverts: equal
    // scores are not hypothetical (identical digests give identical vectors, which
    // devva produces), and the k-th row would quietly become a different record.
    let mut heap: std::collections::BinaryHeap<Scored> =
        std::collections::BinaryHeap::with_capacity(k + 1);
    // Hydration truncation flag (M2.3). Because hydration walks `cands` in score
    // order, whatever is in `out` is always the highest-scoring passers so far —
    // so if the budget cuts here, the partial is PREFIX-CORRECT (an exact prefix
    // of the true answer), not an arbitrary sample. That is why we DEGRADE rather
    // than fail: a very selective residual (fewer than k passers) forces hydrating
    // the whole bucket, but that phase is bucket-BOUNDED, not a runaway scan, so a
    // legitimate small answer must never become an Err. The airbag stays a latency
    // wall (it bounds wall-clock), never a recall wall (it never fails a query).
    let mut hydration_truncated = false;
    // M2.3 budget_stop counters: `candidates` = the whole score-ordered set;
    // `examined` counts ONLY the hydration tail (not the scoring pass, whose
    // `scanned` is shared), so "examined E of C" reads as "hydrated E of C
    // scored". Surfaced only when the airbag actually cuts (below).
    let candidates = cands.len();
    let mut examined = 0usize;
    for c in cands {
        if has_residual && heap.len() == k {
            break; // k winners found — stop hydrating (the whole point).
        }
        scanned += 1;
        if budget_ms > 0
            && scanned.is_multiple_of(BUDGET_CHECK_STRIDE)
            && started.elapsed().as_millis() as u64 >= budget_ms
        {
            // Budget reached mid-hydration: stop and return the score-ordered
            // prefix collected so far, flagged truncated. (The scoring passes are
            // the runaway-scan case and still hard-fail via NearestBudgetExceeded;
            // only this bounded hydration tail degrades.)
            hydration_truncated = true;
            break;
        }
        examined += 1;
        // `range_stream` (the bloom-less scan path) just yielded this key, so it IS
        // live. The bloom-gated point-get should find it — EXCEPT after an unclean
        // crash, where a post-recovery SSTable can carry a bloom that disagrees with
        // its data (recovery-bloom root defect; ticket filed). Then `get` returns
        // None while the scan sees the key: the read-path "survivor key vanished".
        // Fallback bounded to this exact miss: re-read bloom-less (the scan already
        // proved the key exists). LOUD — a hit means the recovery defect is live in
        // production; a bloom-less miss too would be a durability loss (measured
        // 0/18, so it must not happen).
        let blob = match engine
            .turba
            .spatial
            .get(&c.key)
            .map_err(|e| XyzError::Storage(e.to_string()))?
        {
            Some(blob) => blob,
            None => {
                let recovered = engine
                    .turba
                    .spatial
                    .get_no_bloom(&c.key)
                    .map_err(|e| XyzError::Storage(e.to_string()))?;
                match recovered {
                    Some(blob) => {
                        // Co-observation: the SAME key the scan saw and the bloom-gated
                        // get missed IS present (bytes > 0) via the bloom-less read.
                        tracing::warn!(
                            key_len = c.key.len(),
                            recovered_bytes = blob.len(),
                            "nearest hydration: bloom false-negative after crash recovery — \
                             scan saw the key, bloom-gated get missed it, bloom-less get \
                             recovered it; read-path, not durability (see recovery-bloom ticket)"
                        );
                        blob
                    }
                    None => {
                        return Err(XyzError::Storage(
                            "vector prefix: survivor key absent even bloom-less (durability?)"
                                .into(),
                        ));
                    }
                }
            }
        };
        let mut record = deserialize_record(&blob, &lobe_name, fd)
            .map_err(|e| XyzError::Storage(e.to_string()))?;
        if has_residual {
            // Apply the COMPLETE filter (gravity + residual) in one check: it drops
            // both the residual non-matches AND any hash-collision intruder (a
            // foreign gravity value fails the gravity predicate), so the fused
            // residual path needs no separate collision guard. `apply_residual` is
            // false only under the satellite negative-control knob, which lets a
            // hash16 collider leak so the gate can prove the residual earns its keep.
            if apply_residual && !crate::ops::record_matches_opt_expr(&record, &scan.filter_expr) {
                continue;
            }
            // The column was not retained (would rebuild the balloon); re-fetch it
            // by key for this passer and hydrate. A legacy V3/V4 record carries the
            // vector inline (its vectors-keyspace slot is empty) → `get` returns
            // None, nothing to attach.
            if let Some(dict) = fd
                && let Some(column) = engine
                    .turba
                    .vectors
                    .get(&c.key)
                    .map_err(|e| XyzError::Storage(e.to_string()))?
            {
                xyzdb_core::record::hydrate_vector(&mut record, &column, dict);
            }
        } else if let (Some(column), Some(dict)) = (&c.column, fd) {
            // V5 survivor: re-attach the vector from the column bytes read during
            // the scan (no extra point-get). V3/V4 survivors carry it inline.
            xyzdb_core::record::hydrate_vector(&mut record, column, dict);
        }
        heap.push(Scored {
            score: c.score,
            rec: record,
        });
        if heap.len() > k {
            heap.pop(); // drops the worst; `Scored`'s Ord surfaces it
        }
    }
    drop(fr_guard);

    // Re-establish the output order EXPLICITLY. Identical to the full path's final
    // sort (`execute_nearest`): most similar first, ties by ascending lid.
    let mut scored = heap.into_vec();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(a.rec.lid.cmp(&b.rec.lid))
    });
    let out: Vec<Record> = scored.into_iter().map(|s| s.rec).collect();
    // budget_stop is Some ONLY when the airbag cut the hydration tail — the one
    // case where `has_more=true` is a budget stop, not a resumable page. Turns
    // "there may be more" into "examined E of C candidates, found F".
    let budget_stop = hydration_truncated.then(|| xyzdb_core::result::BudgetStop {
        examined,
        candidates,
        found: out.len(),
    });
    Ok(Some((out, budget_stop)))
}

/// Cheap top-k heap entry for the fused fast path: the similarity score, the
/// record `lid` (for the deterministic tiebreak), the 22-byte spatial `key` (so
/// the survivors are re-fetched and deserialized after the scan — NOT a
/// per-record blob clone), and the V5 `column` bytes when they came from the
/// `vectors` keyspace (so a survivor's vector is hydrated from bytes already
/// read, with no extra point-get; `None` on the V3/V4 fallback where the vector
/// is inline in the blob). Only k entries are retained, so the column clones are
/// bounded by k — negligible against the per-query balloon the column removes.
/// Ordered exactly like [`Scored`] so the bounded max-heap surfaces the worst
/// candidate for `pop()`: smaller score is "greater"; tie on score → higher lid
/// is "greater" (dropped first).
struct PrefixCand {
    score: f64,
    lid: xyzdb_core::lid::LID,
    key: Vec<u8>,
    column: Option<Vec<u8>>,
}

impl PartialEq for PrefixCand {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for PrefixCand {}
impl PartialOrd for PrefixCand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PrefixCand {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(Ordering::Equal)
            .then(self.lid.cmp(&other.lid))
    }
}

#[cfg(test)]
mod prune_profile;
#[cfg(test)]
mod scan_profile;
