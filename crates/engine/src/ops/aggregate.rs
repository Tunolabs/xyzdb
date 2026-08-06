// SPDX-License-Identifier: BUSL-1.1
use crate::ops::{CoreFilterExpr, matches_core_expr, to_core_expr};
use std::collections::BTreeMap;
use xytalk_parser::ast::{Aggregate, AggregateFunc};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::record::Record;
use xyzdb_core::result::QueryResult;
use xyzdb_core::value::Value;

/// The canonical result-column label for an aggregate function with no `AS`
/// alias — the SINGLE naming scheme, emitted by both the runtime and the ghost
/// paths (`count`, `sum(field)`, `avg(field)`, `min(field)`, `max(field)`). An
/// aliased metric emits its alias on both paths instead. No translation layer:
/// the ghost stores exactly the label a client sees from the runtime.
pub(crate) fn canonical_label(func: &AggregateFunc) -> String {
    match func {
        AggregateFunc::Count => "count".to_string(),
        AggregateFunc::Sum(f) => format!("sum({f})"),
        AggregateFunc::Avg(f) => format!("avg({f})"),
        AggregateFunc::Min(f) => format!("min({f})"),
        AggregateFunc::Max(f) => format!("max({f})"),
    }
}

/// Resolve one metric's result-column label: its `AS` alias, else `canonical`.
pub(crate) fn resolve_label(
    agg: &Aggregate,
    canonical: impl Fn(&AggregateFunc) -> String,
) -> String {
    agg.alias.clone().unwrap_or_else(|| canonical(&agg.func))
}

/// Resolve and validate the labels of an `AGGREGATE` clause, using `canonical`
/// for the no-alias case. Two rules keep the result columns unambiguous:
///
/// * a filtered `count()` must carry an alias — without one it would collide
///   with the group total, silently relabelling a conditional count as the
///   whole-set count;
/// * no two metrics may resolve to the same label.
///
/// Returns the labels in clause order. Shared by the ghost and runtime paths so
/// both reject the same malformed clauses.
pub(crate) fn resolve_labels(
    aggs: &[Aggregate],
    canonical: impl Fn(&AggregateFunc) -> String,
) -> std::result::Result<Vec<String>, String> {
    let mut labels = Vec::with_capacity(aggs.len());
    let mut seen = std::collections::HashSet::new();
    for a in aggs {
        if a.filter.is_some() && matches!(a.func, AggregateFunc::Count) && a.alias.is_none() {
            return Err(
                "a filtered count() needs an AS alias to distinguish it from the group total"
                    .to_string(),
            );
        }
        let label = resolve_label(a, &canonical);
        if !seen.insert(label.clone()) {
            return Err(format!(
                "duplicate aggregate label '{label}'; add AS <alias> to disambiguate"
            ));
        }
        labels.push(label);
    }
    Ok(labels)
}

/// Execute AGGREGATE over a set of records. Each metric folds only the records
/// passing its per-metric filter and lands under its resolved label.
pub fn execute_aggregate(records: &[Record], aggs: &[Aggregate]) -> Result<QueryResult> {
    let labels = resolve_labels(aggs, canonical_label).map_err(XyzError::Parse)?;
    let mut results = BTreeMap::new();

    for (agg, label) in aggs.iter().zip(labels) {
        let core = agg.filter.as_ref().map(to_core_expr);
        let matching: Vec<&Record> = match &core {
            Some(c) => records.iter().filter(|r| matches_core_expr(r, c)).collect(),
            None => records.iter().collect(),
        };
        match &agg.func {
            AggregateFunc::Count => {
                results.insert(label, Value::Int(matching.len() as i64));
            }
            AggregateFunc::Sum(field) => {
                let sum: f64 = matching
                    .iter()
                    .filter_map(|r| r.fields.get(field))
                    .filter_map(numeric_value)
                    .sum();
                results.insert(label, Value::Float(sum));
            }
            AggregateFunc::Avg(field) => {
                let values: Vec<f64> = matching
                    .iter()
                    .filter_map(|r| r.fields.get(field))
                    .filter_map(numeric_value)
                    .collect();
                let avg = if values.is_empty() {
                    0.0
                } else {
                    values.iter().sum::<f64>() / values.len() as f64
                };
                results.insert(label, Value::Float(avg));
            }
            AggregateFunc::Min(field) => {
                let min = matching
                    .iter()
                    .filter_map(|r| r.fields.get(field))
                    .filter_map(numeric_value)
                    .fold(f64::INFINITY, f64::min);
                if min.is_finite() {
                    results.insert(label, Value::Float(min));
                }
            }
            AggregateFunc::Max(field) => {
                let max = matching
                    .iter()
                    .filter_map(|r| r.fields.get(field))
                    .filter_map(numeric_value)
                    .fold(f64::NEG_INFINITY, f64::max);
                if max.is_finite() {
                    results.insert(label, Value::Float(max));
                }
            }
        }
    }

    Ok(QueryResult::Aggregation(results))
}

fn numeric_value(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

// ─── Incremental aggregation (no Vec<Record> accumulation) ──────────────────

/// One resolved metric in a streaming accumulator: its function, its per-metric
/// filter compiled once to the walker form, and its resolved result label.
#[derive(Clone)]
struct ResolvedAgg {
    func: AggregateFunc,
    core: Option<CoreFilterExpr>,
    label: String,
}

/// Accumulator state for streaming aggregation. Keyed by resolved label, so
/// same-op metrics with different per-metric filters accumulate independently.
#[derive(Clone)]
pub struct AggAccumulator {
    resolved: Vec<ResolvedAgg>,
    counts: BTreeMap<String, i64>,
    sums: BTreeMap<String, f64>,
    avg_counts: BTreeMap<String, u64>,
    mins: BTreeMap<String, f64>,
    maxs: BTreeMap<String, f64>,
}

impl AggAccumulator {
    /// Build from a validated clause. Compiles each per-metric filter once;
    /// caller must have run [`resolve_labels`] so labels are unambiguous.
    pub fn new(aggs: Vec<Aggregate>) -> Self {
        let resolved = aggs
            .iter()
            .map(|a| ResolvedAgg {
                func: a.func.clone(),
                core: a.filter.as_ref().map(to_core_expr),
                label: resolve_label(a, canonical_label),
            })
            .collect();
        Self {
            resolved,
            counts: BTreeMap::new(),
            sums: BTreeMap::new(),
            avg_counts: BTreeMap::new(),
            mins: BTreeMap::new(),
            maxs: BTreeMap::new(),
        }
    }

    /// Feed one record into the accumulator.
    /// Supports dot notation via resolve_path (e.g. "scoring.bureau").
    pub fn observe(&mut self, record: &Record) {
        use xyzdb_core::record::resolve_path;
        for ra in &self.resolved {
            if let Some(c) = &ra.core
                && !matches_core_expr(record, c)
            {
                continue;
            }
            match &ra.func {
                AggregateFunc::Count => {
                    *self.counts.entry(ra.label.clone()).or_insert(0) += 1;
                }
                AggregateFunc::Sum(field) => {
                    if let Some(v) = resolve_path(&record.fields, field).and_then(numeric_value) {
                        *self.sums.entry(ra.label.clone()).or_insert(0.0) += v;
                    }
                }
                AggregateFunc::Avg(field) => {
                    if let Some(v) = resolve_path(&record.fields, field).and_then(numeric_value) {
                        *self.sums.entry(ra.label.clone()).or_insert(0.0) += v;
                        *self.avg_counts.entry(ra.label.clone()).or_insert(0) += 1;
                    }
                }
                AggregateFunc::Min(field) => {
                    if let Some(v) = resolve_path(&record.fields, field).and_then(numeric_value) {
                        let min = self.mins.entry(ra.label.clone()).or_insert(f64::INFINITY);
                        if v < *min {
                            *min = v;
                        }
                    }
                }
                AggregateFunc::Max(field) => {
                    if let Some(v) = resolve_path(&record.fields, field).and_then(numeric_value) {
                        let max = self
                            .maxs
                            .entry(ra.label.clone())
                            .or_insert(f64::NEG_INFINITY);
                        if v > *max {
                            *max = v;
                        }
                    }
                }
            }
        }
    }

    /// Finalize into a QueryResult.
    pub fn finalize(self) -> QueryResult {
        let mut results = BTreeMap::new();
        for ra in &self.resolved {
            match &ra.func {
                AggregateFunc::Count => {
                    let count = self.counts.get(&ra.label).copied().unwrap_or(0);
                    results.insert(ra.label.clone(), Value::Int(count));
                }
                AggregateFunc::Sum(_) => {
                    let sum = self.sums.get(&ra.label).copied().unwrap_or(0.0);
                    results.insert(ra.label.clone(), Value::Float(sum));
                }
                AggregateFunc::Avg(_) => {
                    let sum = self.sums.get(&ra.label).copied().unwrap_or(0.0);
                    let count = self.avg_counts.get(&ra.label).copied().unwrap_or(0);
                    let avg = if count == 0 { 0.0 } else { sum / count as f64 };
                    results.insert(ra.label.clone(), Value::Float(avg));
                }
                AggregateFunc::Min(_) => {
                    if let Some(&min) = self.mins.get(&ra.label)
                        && min.is_finite()
                    {
                        results.insert(ra.label.clone(), Value::Float(min));
                    }
                }
                AggregateFunc::Max(_) => {
                    if let Some(&max) = self.maxs.get(&ra.label)
                        && max.is_finite()
                    {
                        results.insert(ra.label.clone(), Value::Float(max));
                    }
                }
            }
        }
        QueryResult::Aggregation(results)
    }
}

/// Deterministic string key for GROUP BY grouping.
/// Owned format — not dependent on Rust Debug trait stability.
pub fn canonical_key(value: Option<&Value>) -> String {
    match value {
        None => "N".to_string(),
        Some(Value::Null) => "N".to_string(),
        Some(Value::Bool(b)) => format!("B{}", *b as u8),
        Some(Value::Int(n)) => format!("I{n}"),
        Some(Value::Float(f)) => format!("F{f:?}"),
        Some(Value::Text(s)) => format!("T{s}"),
        Some(Value::Timestamp(t)) => format!("S{t}"),
        Some(Value::Bytes(b)) => format!("X{}", b.len()),
        Some(Value::List(l)) => format!("L{}", l.len()),
        Some(Value::Map(m)) => format!("M{}", m.len()),
        Some(Value::Vector(v)) => format!("V{}", v.len()),
    }
}
