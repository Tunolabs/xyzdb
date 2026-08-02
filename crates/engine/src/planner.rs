use crate::engine::{Engine, QueryResult};
use std::collections::BTreeMap;
use xytalk_parser::ast::{PipelineStep, TopBy, TopStmt};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::record::Record;
use xyzdb_core::value::Value;

/// Execute a pipeline: chain of operations where each feeds into the next.
pub fn execute_pipeline(engine: &Engine, steps: Vec<PipelineStep>) -> Result<QueryResult> {
    // Special case: SCAN | AGGREGATE — incremental aggregation without Vec<Record>.
    // This allows count/sum/avg over millions of records without OOM.
    if steps.len() == 2
        && let [
            PipelineStep::Scan(scan_stmt),
            PipelineStep::Aggregate(funcs),
        ] = &steps[..]
    {
        return crate::ops::scan::execute_scan_aggregate(engine, scan_stmt.clone(), funcs.clone());
    }

    // Special case: SCAN | NEAREST [| …] — fused V3 vector-prefix fast path.
    // Reads only the hoisted vector prefix per record in the gravity bucket,
    // ranks top-k, and fully deserializes only the survivors. Internally falls
    // back to the exact scan→execute_nearest path when the prefix path can't
    // apply, so the result is always identical to the unfused pipeline.
    //
    // It fires whenever the pipeline STARTS with these two steps, not only when
    // it is exactly these two. A longer pipeline used to fall to the generic
    // loop below, where `SCAN` materialises one default page (1000 records) and
    // `NEAREST` ranks within it: appending `| SHAPE {id}` — a projection, which
    // by definition cannot change which records come back — silently turned a
    // top-k over the whole 24,943-row bucket into a top-k over the first 1000.
    // Measured on the benchmark corpus: five entirely different ids, `status:
    // ok`, no flag. Any tail steps run below on the fused result.
    if let [
        PipelineStep::Scan(scan_stmt),
        PipelineStep::Nearest(nearest_stmt),
        ..,
    ] = &steps[..]
    {
        let (scan_stmt, nearest_stmt) = (scan_stmt.clone(), nearest_stmt.clone());
        let (records, budget_stop) =
            crate::ops::nearest::execute_scan_nearest(engine, scan_stmt, &nearest_stmt)?;
        if steps.len() > 2 {
            let mut steps = steps;
            let tail = steps.split_off(2);
            return run_steps(engine, tail, Some(records), budget_stop);
        }
        if budget_stop.is_some() {
            // Budget cut the score-ordered hydration: `records` are the
            // highest-scoring passers found within budget — a prefix-correct
            // partial, NOT an arbitrary sample. Surface it via the existing
            // truncation channel with `has_more = true`, but `cursor: None`:
            // NEAREST has no resumable page (resuming would repeat the whole
            // scoring pass), so the flag is a plain "these are the best found,
            // more lower-scoring ones may exist" — deliberately no SCAN cursor.
            // `budget_stop` carries examined/candidates/found — this is the ONLY
            // site that fills it (the only path that truncates on the airbag).
            return Ok(QueryResult::PaginatedRecords {
                records,
                cursor: None,
                has_more: true,
                budget_stop,
            });
        }
        return Ok(QueryResult::Records(records));
    }

    // Special case: SCAN | GROUP BY | AGGREGATE — per-group accumulation.
    if steps.len() == 3
        && let [
            PipelineStep::Scan(scan_stmt),
            PipelineStep::GroupBy(fields),
            PipelineStep::Aggregate(funcs),
        ] = &steps[..]
    {
        return crate::ops::scan::execute_scan_group_aggregate(
            engine,
            scan_stmt.clone(),
            fields.clone(),
            funcs.clone(),
            None,
        );
    }

    // Special case: SCAN | GROUP BY | AGGREGATE | TOP — server-side top-N over
    // the grouped result. Runs the group-aggregate (ghost PreComputed or
    // Primary, identically), then selects the N best groups by a metric. The
    // grouped rows are already materialised in memory (read_precomputed / the
    // runtime accumulator return every group), so this adds no I/O — it
    // replaces "transfer M groups + client sort" with a server-side partial
    // selection + transfer N.
    if steps.len() == 4
        && let [
            PipelineStep::Scan(scan_stmt),
            PipelineStep::GroupBy(fields),
            PipelineStep::Aggregate(funcs),
            PipelineStep::Top(top),
        ] = &steps[..]
    {
        // Pass `top` down so the group-aggregate path can serve O(N) from a
        // ghost's metric-ordered rollup when the ghost declared `ORDER BY` the
        // same metric. When it can't, it materialises all M groups and `apply_top`
        // does the O(M) quickselect below. `apply_top` runs either way: on an
        // O(N) result it is idempotent (already the top-N in order); on the O(M)
        // result it selects. So the served result is identical to sort-all.
        let grouped = crate::ops::scan::execute_scan_group_aggregate(
            engine,
            scan_stmt.clone(),
            fields.clone(),
            funcs.clone(),
            Some(top),
        )?;
        return apply_top(grouped, top, fields);
    }

    run_steps(engine, steps, None, None)
}

/// Run a pipeline step by step over an optional seed of records.
///
/// `seed` is the output of a fused prefix that already ran (today: the fused
/// `SCAN | NEAREST`); `budget_stop` is that prefix's airbag report, carried to
/// the end so a partial candidate set still announces itself after the tail
/// steps have run.
fn run_steps(
    engine: &Engine,
    steps: Vec<PipelineStep>,
    seed: Option<Vec<Record>>,
    budget_stop: Option<xyzdb_core::result::BudgetStop>,
) -> Result<QueryResult> {
    let mut current_records: Option<Vec<Record>> = seed;

    for step in steps {
        // A step that consumes the records and returns its own result (SET,
        // DELETE, AGGREGATE) would swallow the airbag report, and the caller
        // would see a mutation or a total that silently covered only the part
        // the budget managed to score. Refuse instead: the flag has nowhere to
        // travel on those results, so the only honest answer is to say so.
        if budget_stop.is_some()
            && matches!(
                step,
                PipelineStep::Set(_) | PipelineStep::Delete(_) | PipelineStep::Aggregate(_)
            )
        {
            return Err(XyzError::InvalidQuery(
                "NEAREST hit --nearest-budget-ms and returned a partial candidate set; \
                 refusing to run a mutating or aggregating step over it. Narrow the scope \
                 or raise the budget."
                    .into(),
            ));
        }
        match step {
            PipelineStep::Find(f) => {
                let result = crate::ops::find::execute_find(engine, f)?;
                current_records = Some(extract_records(result)?);
            }
            PipelineStep::Scan(s) => {
                let scan_result = crate::ops::scan::execute_scan(engine, s)?;
                current_records = Some(extract_records(scan_result.query_result)?);
            }
            PipelineStep::ScanGhost(s) => {
                let result = engine.execute(xytalk_parser::ast::Statement::ScanGhost(s))?;
                current_records = Some(extract_records(result)?);
            }
            PipelineStep::Pull(p) => {
                let recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("PULL in pipeline requires preceding records".into())
                })?;
                let result = crate::ops::pull::execute_pull(engine, p, Some(recs))?;
                current_records = Some(extract_records(result)?);
            }
            PipelineStep::Set(s) => {
                let recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("SET in pipeline requires preceding records".into())
                })?;
                let result = crate::ops::set::execute_set(engine, s, Some(recs))?;
                return Ok(result);
            }
            PipelineStep::Delete(d) => {
                let recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("DELETE in pipeline requires preceding records".into())
                })?;
                let result = crate::ops::delete::execute_delete(engine, d, Some(recs))?;
                return Ok(result);
            }
            PipelineStep::Aggregate(funcs) => {
                let recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("AGGREGATE requires preceding records".into())
                })?;
                return crate::ops::aggregate::execute_aggregate(&recs, &funcs);
            }
            PipelineStep::GroupBy(_) => {
                return Err(XyzError::InvalidQuery(
                    "GROUP BY is only supported as SCAN | GROUP BY field | AGGREGATE funcs()"
                        .into(),
                ));
            }
            PipelineStep::Nearest(stmt) => {
                let recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("NEAREST requires preceding records".into())
                })?;
                current_records = Some(crate::ops::nearest::execute_nearest(recs, &stmt)?);
            }
            PipelineStep::Follow(stmt) => {
                let recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("FOLLOW requires preceding records".into())
                })?;
                current_records = Some(crate::ops::follow::execute_follow(engine, recs, &stmt)?);
            }
            PipelineStep::Top(top) => {
                // `TAKE n` (no BY) truncates the current record stream — the
                // pipeline form of LIMIT. `TAKE n BY metric` needs the grouped
                // aggregate (handled by the 4-step fast path above), so it's an
                // error here.
                if top.by.is_some() {
                    return Err(XyzError::InvalidQuery(
                        "TAKE n BY metric needs SCAN | GROUP BY … | AGGREGATE … | TAKE n BY metric"
                            .into(),
                    ));
                }
                let mut recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("TAKE requires preceding records".into())
                })?;
                recs.truncate(top.n as usize);
                current_records = Some(recs);
            }
            PipelineStep::Shape(shape) => {
                // Projection: keep only the named fields on each record. Names
                // absent from a record are simply not present in the result —
                // a projection, not a filter. Structural identity (lid, lobe,
                // timestamps) is untouched.
                let mut recs = current_records.take().ok_or_else(|| {
                    XyzError::InvalidQuery("SHAPE requires preceding records".into())
                })?;
                let keep: std::collections::HashSet<&str> =
                    shape.fields.iter().map(String::as_str).collect();
                for rec in recs.iter_mut() {
                    rec.fields.retain(|k, _| keep.contains(k.as_str()));
                }
                current_records = Some(recs);
            }
        }
    }

    // Pipeline ended with records (FIND | PULL, or just FIND)
    match (current_records, budget_stop) {
        // A fused prefix hit the airbag: the tail ran over a partial candidate
        // set, so the answer still travels through the truncation channel. The
        // tail cannot restore what the airbag never scored, and a partial that
        // arrives looking complete is the one failure this flag exists to
        // prevent.
        (Some(records), Some(stop)) => Ok(QueryResult::PaginatedRecords {
            records,
            cursor: None,
            has_more: true,
            budget_stop: Some(stop),
        }),
        (Some(records), None) => Ok(QueryResult::Records(records)),
        (None, _) => Ok(QueryResult::Records(vec![])),
    }
}

/// Keep the `top.n` best groups by the chosen metric. Total order:
/// primary = metric value (DESC by default, ASC if requested), secondary =
/// group key ascending. Group keys are unique (one row per group), so the order
/// is strict — ties at the N/N+1 cut are broken deterministically and the result
/// equals sort-all-then-truncate. Uses `select_nth_unstable_by` (quickselect,
/// O(M) partial selection) instead of a full O(M log M) sort — only the N
/// survivors are then ordered.
fn apply_top(result: QueryResult, top: &TopStmt, group_fields: &[String]) -> Result<QueryResult> {
    let mut rows = match result {
        QueryResult::GroupedAggregation(rows) => rows,
        // execute_scan_group_aggregate always yields GroupedAggregation; anything
        // else means an upstream contract changed — surface it, don't mask it.
        other => return Ok(other),
    };
    // `TAKE n` without BY: truncate the grouped rows to the first n, no reorder.
    let by = match &top.by {
        Some(by) => by,
        None => {
            rows.truncate(top.n as usize);
            return Ok(QueryResult::GroupedAggregation(rows));
        }
    };
    let label = match by {
        TopBy::Metric(f) => crate::ops::aggregate::canonical_label(f),
        TopBy::Alias(a) => a.clone(),
    };
    // BY must name a metric actually produced by the AGGREGATE clause.
    if let Some(first) = rows.first()
        && !first.contains_key(&label)
    {
        return Err(XyzError::InvalidQuery(format!(
            "TOP BY '{label}': not a metric in the AGGREGATE clause"
        )));
    }

    // The metric + tiebreak used here are the SAME functions the metric-ordered
    // rollup emits and reads by, so the O(N) order equals this comparator exactly.
    let metric =
        |r: &BTreeMap<String, Value>| crate::ghost::metric_order::top_metric_f64(r, &label);
    let key =
        |r: &BTreeMap<String, Value>| crate::ghost::metric_order::top_tiebreak_key(group_fields, r);
    let desc = top.descending;
    // `cmp(a, b) == Less` means a ranks ahead of b (belongs earlier in the top-N).
    let cmp = |a: &BTreeMap<String, Value>, b: &BTreeMap<String, Value>| {
        let (ma, mb) = (metric(a), metric(b));
        let primary = if desc {
            mb.total_cmp(&ma)
        } else {
            ma.total_cmp(&mb)
        };
        primary.then_with(|| key(a).cmp(&key(b)))
    };

    let n = top.n as usize;
    if n < rows.len() {
        // Partition so rows[0..n] are the n best (unordered), O(M) average.
        rows.select_nth_unstable_by(n, &cmp);
        rows.truncate(n);
    }
    // Order the survivors (≤ n) for a deterministic result.
    rows.sort_by(&cmp);
    Ok(QueryResult::GroupedAggregation(rows))
}

fn extract_records(result: QueryResult) -> Result<Vec<Record>> {
    match result {
        QueryResult::Records(recs) => Ok(recs),
        // v0.2.5.1: pagination metadata is consumed at the pipeline boundary.
        // Middle steps (PULL/SET/DELETE) operate on the returned page; the
        // caller cannot resume mid-pipeline anyway.
        QueryResult::PaginatedRecords { records, .. } => Ok(records),
        QueryResult::Ok { message, .. } => Err(XyzError::InvalidQuery(format!(
            "Expected records in pipeline, got: {message}"
        ))),
        QueryResult::BatchOk { .. } => Err(XyzError::InvalidQuery(
            "Expected records in pipeline, got batch result".into(),
        )),
        QueryResult::Aggregation(_) => Err(XyzError::InvalidQuery(
            "Expected records in pipeline, got aggregation".into(),
        )),
        QueryResult::GroupedAggregation(_) => Err(XyzError::InvalidQuery(
            "Expected records in pipeline, got grouped aggregation".into(),
        )),
        QueryResult::Info(_) => Err(XyzError::InvalidQuery(
            "Expected records in pipeline, got info".into(),
        )),
    }
}
