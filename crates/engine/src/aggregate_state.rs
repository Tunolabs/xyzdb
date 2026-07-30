//! Incremental aggregate state for Ghost V2.
//!
//! Supports add/subtract for count, sum, avg. Min/Max are add-only;
//! subtract marks them dirty for periodic reconciliation.

use crate::ops::{CoreFilterExpr, matches_core_expr, to_core_expr};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xytalk_parser::ast::FilterExpr;
use xyzdb_core::record::Record;
use xyzdb_core::value::Value;

/// The reserved result label for a group's total record count. Always emitted by
/// [`AggregateState::to_result`] from `self.count`; an unfiltered `count()`
/// metric rides it (no separate accumulator) rather than duplicating the total.
pub const COUNT_LABEL: &str = "count";

/// One aggregate metric: an op over a field, gated by an optional per-metric
/// filter, emitted under a resolved column `label`.
///
/// This replaces the field-grouped `AggregateSpec { field, ops }`: identity is
/// now per-metric (by `label`), so several metrics of the same op with different
/// filters (e.g. three `count()` over disjoint predicates) no longer collide in
/// the aggregate map. `filter` is the persistable source of truth; `filter_core`
/// is the walker-ready form, built once at construction so the per-write path
/// evaluates without re-converting the AST (preserves the P2-2 no-reconversion
/// guarantee the ghost header filter already has).
#[derive(Clone, Debug)]
pub struct Metric {
    /// The aggregated field. Empty for `count()` (fieldless).
    pub field: String,
    pub op: AggOp,
    /// Resolved result-column label: the `AS` alias, else the canonical
    /// `field:Op` (or [`COUNT_LABEL`] for `count()`).
    pub label: String,
    /// Per-metric `WHERE`, source of truth for persistence. `None` = unconditional.
    pub filter: Option<FilterExpr>,
    /// Walker-ready filter, built once from `filter`. `None` = unconditional.
    pub filter_core: Option<CoreFilterExpr>,
}

impl Metric {
    /// Build a metric, compiling `filter` to its walker-ready form once.
    pub fn new(field: String, op: AggOp, label: String, filter: Option<FilterExpr>) -> Self {
        let filter_core = filter.as_ref().map(to_core_expr);
        Self {
            field,
            op,
            label,
            filter,
            filter_core,
        }
    }

    /// True when this metric contributes only to the group's total count and so
    /// rides `AggregateState::count` instead of its own accumulator: an
    /// unconditional `count()` labelled with the reserved [`COUNT_LABEL`].
    fn rides_total_count(&self) -> bool {
        matches!(self.op, AggOp::Count) && self.label == COUNT_LABEL
    }

    /// Whether a record passes this metric's per-metric filter.
    fn matches(&self, record: &Record) -> bool {
        self.filter_core
            .as_ref()
            .is_none_or(|core| matches_core_expr(record, core))
    }
}

/// Aggregate operations.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum AggOp {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

/// A stable, order-independent signature of a metric set: one
/// `op␁field␁label␁filter` string per metric, sorted. Two `AGGREGATE` clauses
/// produce the same element for a metric iff they request the same op, field,
/// result label, and per-metric filter. The router uses set-containment over
/// these (a ghost may serve an aggregate query only when its signature COVERS
/// the query's — i.e. it precomputes every requested metric) so a query is never
/// served a ghost that lacks a metric or precomputes it under a different filter.
pub fn aggregate_signature(metrics: &[Metric]) -> Vec<String> {
    let mut sig: Vec<String> = metrics
        .iter()
        .map(|m| {
            format!(
                "{:?}\u{1}{}\u{1}{}\u{1}{:?}",
                m.op, m.field, m.label, m.filter
            )
        })
        .collect();
    sig.sort();
    sig
}

/// A single aggregate value, updated incrementally.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum AggValue {
    Count(u64),
    Sum(SumAcc),
    Min(StoredValue),
    Max(StoredValue),
    Avg { sum: SumAcc, count: u64 },
}

/// Sum accumulator, typed by the values it has seen.
///
/// Integer values (money as cents) accumulate exactly in `i128`; the first
/// float promotes the accumulator to a Neumaier-compensated `f64`. Both add and
/// subtract are exact for integers and compensated for floats, so the delta
/// path (subtract-old, add-new on every update/delete) no longer composes the
/// rounding error the naive `f64 += / -=` did — a large intermediate value that
/// swamps small addends is recovered instead of silently dropped.
///
/// Output is still `Value::Float` (parity with the runtime aggregate path);
/// `i128` protects the *accumulation*, not the final one-shot cast.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SumAcc {
    /// Exact integer accumulation. `i128` gives headroom over summed `i64`s.
    Int(i128),
    /// Neumaier-compensated float accumulation; the value is `sum + comp`.
    Float { sum: f64, comp: f64 },
}

impl Default for SumAcc {
    fn default() -> Self {
        SumAcc::Int(0)
    }
}

/// Neumaier's compensated addition: fold `x` into `(sum, comp)` capturing the
/// low-order bits lost to rounding. Handles negative `x` (subtraction) too.
fn neumaier_add(sum: &mut f64, comp: &mut f64, x: f64) {
    let t = *sum + x;
    if sum.abs() >= x.abs() {
        *comp += (*sum - t) + x;
    } else {
        *comp += (x - t) + *sum;
    }
    *sum = t;
}

impl SumAcc {
    /// Add `value * sign` to the accumulator. `sign` is `+1` (add) or `-1`
    /// (subtract); subtraction is addition of the negated value. Non-numeric
    /// values are ignored (matching the old `as_f64` returning `None`).
    fn add_scaled(&mut self, value: &Value, sign: i64) {
        match value {
            Value::Int(i) => {
                let delta = (*i as i128) * (sign as i128);
                match self {
                    SumAcc::Int(acc) => *acc += delta,
                    SumAcc::Float { sum, comp } => neumaier_add(sum, comp, delta as f64),
                }
            }
            Value::Float(f) => {
                let delta = *f * (sign as f64);
                // The first float promotes an integer accumulator to float.
                if let SumAcc::Int(acc) = self {
                    *self = SumAcc::Float {
                        sum: *acc as f64,
                        comp: 0.0,
                    };
                }
                if let SumAcc::Float { sum, comp } = self {
                    neumaier_add(sum, comp, delta);
                }
            }
            _ => {}
        }
    }

    /// Fold another accumulator into this one (rollup merge / partial fold).
    fn merge(&mut self, other: &SumAcc) {
        match other {
            SumAcc::Int(b) => match self {
                SumAcc::Int(a) => *a += b,
                SumAcc::Float { sum, comp } => neumaier_add(sum, comp, *b as f64),
            },
            SumAcc::Float { sum: os, comp: oc } => {
                if let SumAcc::Int(a) = self {
                    *self = SumAcc::Float {
                        sum: *a as f64,
                        comp: 0.0,
                    };
                }
                if let SumAcc::Float { sum, comp } = self {
                    neumaier_add(sum, comp, *os);
                    *comp += *oc;
                }
            }
        }
    }

    /// The accumulated value as `f64` (integers cast once; floats add the
    /// compensation term).
    pub fn to_f64(&self) -> f64 {
        match self {
            SumAcc::Int(i) => *i as f64,
            SumAcc::Float { sum, comp } => *sum + *comp,
        }
    }
}

/// Comparable value type for Min/Max tracking.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum StoredValue {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
}

impl PartialOrd for StoredValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (StoredValue::Null, StoredValue::Null) => Some(std::cmp::Ordering::Equal),
            (StoredValue::Null, _) => Some(std::cmp::Ordering::Less),
            (_, StoredValue::Null) => Some(std::cmp::Ordering::Greater),
            (StoredValue::Int(a), StoredValue::Int(b)) => a.partial_cmp(b),
            (StoredValue::Float(a), StoredValue::Float(b)) => a.partial_cmp(b),
            (StoredValue::Text(a), StoredValue::Text(b)) => a.partial_cmp(b),
            (StoredValue::Int(a), StoredValue::Float(b)) => (*a as f64).partial_cmp(b),
            (StoredValue::Float(a), StoredValue::Int(b)) => a.partial_cmp(&(*b as f64)),
            _ => None,
        }
    }
}

impl StoredValue {
    pub fn from_value(v: &Value) -> Self {
        match v {
            Value::Int(i) => StoredValue::Int(*i),
            Value::Float(f) => StoredValue::Float(*f),
            Value::Text(s) => StoredValue::Text(s.clone()),
            Value::Null => StoredValue::Null,
            _ => StoredValue::Null,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            StoredValue::Int(i) => Value::Int(*i),
            StoredValue::Float(f) => Value::Float(*f),
            StoredValue::Text(s) => Value::Text(s.clone()),
            StoredValue::Null => Value::Null,
        }
    }
}

/// Aggregate state: count + per-field aggregate values.
/// Supports incremental add/subtract for ghost maintenance.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AggregateState {
    pub count: u64,
    pub values: BTreeMap<String, AggValue>,
    /// True if Min/Max may be stale (after a subtract of min/max value).
    #[serde(default)]
    pub dirty: bool,
}

impl AggregateState {
    /// Best-effort resident-byte estimate of this state: struct stack size
    /// plus per-entry map overhead plus the heap behind keys and Text
    /// min/max values. Feeds the `/stats` ram_budget observer; the target
    /// is right-order-of-magnitude, not allocator-exact.
    pub fn estimated_bytes(&self) -> usize {
        // Amortised BTreeMap per-entry overhead (node slots + parents).
        const MAP_ENTRY_OVERHEAD: usize = 32;
        let mut bytes = std::mem::size_of::<Self>();
        for (k, v) in &self.values {
            bytes += MAP_ENTRY_OVERHEAD
                + std::mem::size_of::<String>()
                + k.len()
                + std::mem::size_of::<AggValue>();
            if let AggValue::Min(StoredValue::Text(s)) | AggValue::Max(StoredValue::Text(s)) = v {
                bytes += s.len();
            }
        }
        bytes
    }

    /// Add a record's contribution to the aggregates. The record has already
    /// passed the ghost/query header predicate (membership), so `self.count`
    /// (the group total) always increments; each metric folds only if the record
    /// also passes that metric's per-metric filter.
    pub fn add(&mut self, record: &Record, metrics: &[Metric]) {
        self.count += 1;
        for m in metrics {
            if m.rides_total_count() || !m.matches(record) {
                continue;
            }
            match m.op {
                AggOp::Count => {
                    if let AggValue::Count(c) = self
                        .values
                        .entry(m.label.clone())
                        .or_insert(AggValue::Count(0))
                    {
                        *c += 1;
                    }
                }
                _ => {
                    if let Some(value) = record.fields.get(&m.field) {
                        self.values
                            .entry(m.label.clone())
                            .or_insert_with(|| AggValue::new(&m.op))
                            .add(value);
                    }
                }
            }
        }
    }

    /// Subtract a record's contribution from the aggregates.
    /// Count, Sum, Avg are exact. Min/Max are marked dirty.
    pub fn subtract(&mut self, record: &Record, metrics: &[Metric]) {
        self.count = self.count.saturating_sub(1);
        for m in metrics {
            if m.rides_total_count() || !m.matches(record) {
                continue;
            }
            match m.op {
                AggOp::Count => {
                    if let Some(AggValue::Count(c)) = self.values.get_mut(&m.label) {
                        *c = c.saturating_sub(1);
                    }
                }
                _ => {
                    if let Some(value) = record.fields.get(&m.field)
                        && let Some(entry) = self.values.get_mut(&m.label)
                        && entry.subtract(value)
                    {
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Convert to a list of (name, value) pairs for query response.
    pub fn to_result(&self) -> Vec<(String, Value)> {
        let mut result = vec![("count".to_string(), Value::Int(self.count as i64))];
        for (key, agg) in &self.values {
            result.push((key.clone(), agg.to_value()));
        }
        result
    }

    /// Reset all values — used before reconciliation repopulates.
    // parked: aggregation reconciliation/merge
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.count = 0;
        self.values.clear();
        self.dirty = false;
    }

    /// Fold another state into this one. Used to combine the partial
    /// rollups a lightweight ghost spills to disk: every aggregate here
    /// is decomposable (counts and sums add; min/max compare; avg keeps
    /// its sum/count pair), so merging partials is exact.
    // parked: aggregation reconciliation/merge
    #[allow(dead_code)]
    pub fn merge(&mut self, other: &AggregateState) {
        self.count += other.count;
        self.dirty |= other.dirty;
        for (key, theirs) in &other.values {
            match self.values.entry(key.clone()) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(theirs.clone());
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    e.get_mut().merge(theirs);
                }
            }
        }
    }
}

impl AggValue {
    fn new(op: &AggOp) -> Self {
        match op {
            AggOp::Count => AggValue::Count(0),
            AggOp::Sum => AggValue::Sum(SumAcc::default()),
            AggOp::Min => AggValue::Min(StoredValue::Null),
            AggOp::Max => AggValue::Max(StoredValue::Null),
            AggOp::Avg => AggValue::Avg {
                sum: SumAcc::default(),
                count: 0,
            },
        }
    }

    fn add(&mut self, value: &Value) {
        match self {
            AggValue::Count(c) => *c += 1,
            AggValue::Sum(s) => s.add_scaled(value, 1),
            AggValue::Min(current) => {
                let sv = StoredValue::from_value(value);
                if *current == StoredValue::Null || sv < *current {
                    *current = sv;
                }
            }
            AggValue::Max(current) => {
                let sv = StoredValue::from_value(value);
                if *current == StoredValue::Null || sv > *current {
                    *current = sv;
                }
            }
            AggValue::Avg { sum, count } => {
                if as_f64(value).is_some() {
                    sum.add_scaled(value, 1);
                    *count += 1;
                }
            }
        }
    }

    /// Subtract a value. Returns true if Min/Max became dirty.
    fn subtract(&mut self, value: &Value) -> bool {
        match self {
            AggValue::Count(c) => {
                *c = c.saturating_sub(1);
                false
            }
            AggValue::Sum(s) => {
                s.add_scaled(value, -1);
                false
            }
            AggValue::Avg { sum, count } => {
                if as_f64(value).is_some() {
                    sum.add_scaled(value, -1);
                    *count = count.saturating_sub(1);
                }
                false
            }
            // Min/Max: can't subtract incrementally. Mark as dirty.
            AggValue::Min(_) | AggValue::Max(_) => true,
        }
    }

    /// Fold another instance of the same aggregate op into this one.
    /// Mismatched variants (schema drift between partials) keep `self`.
    // parked: aggregation reconciliation/merge
    #[allow(dead_code)]
    fn merge(&mut self, other: &AggValue) {
        match (self, other) {
            (AggValue::Count(a), AggValue::Count(b)) => *a += b,
            (AggValue::Sum(a), AggValue::Sum(b)) => a.merge(b),
            (AggValue::Min(a), AggValue::Min(b)) => {
                if *b != StoredValue::Null && (*a == StoredValue::Null || *b < *a) {
                    *a = b.clone();
                }
            }
            (AggValue::Max(a), AggValue::Max(b)) => {
                if *b != StoredValue::Null && (*a == StoredValue::Null || *b > *a) {
                    *a = b.clone();
                }
            }
            (AggValue::Avg { sum: s, count: c }, AggValue::Avg { sum: os, count: oc }) => {
                s.merge(os);
                *c += oc;
            }
            _ => {}
        }
    }

    fn to_value(&self) -> Value {
        match self {
            AggValue::Count(c) => Value::Int(*c as i64),
            AggValue::Sum(s) => Value::Float(s.to_f64()),
            AggValue::Min(v) | AggValue::Max(v) => v.to_value(),
            AggValue::Avg { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Float(sum.to_f64() / *count as f64)
                }
            }
        }
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

pub type GroupKey = String;

/// Extract a group key string from record fields.
pub fn extract_group_key(fields: &BTreeMap<String, Value>, group_fields: &[String]) -> GroupKey {
    let fragments: Vec<String> = group_fields
        .iter()
        .map(|f| value_to_group_key_fragment(fields.get(f)))
        .collect();
    encode_group_key(&fragments)
}

/// Encode group-key fragments length-prefixed: each fragment as
/// `<byte_len>:<bytes>`, concatenated. No separator to collide with, so a field
/// value containing `|` no longer merges distinct groups — and the per-fragment
/// type tag (see [`value_to_group_key_fragment`]) keeps `Null` distinct from the
/// text `"null"` and `Int(5)` distinct from `"5"`. This is the ghost's group
/// key; the runtime path keys separately.
pub fn encode_group_key(fragments: &[String]) -> GroupKey {
    let mut out = String::new();
    for frag in fragments {
        out.push_str(&frag.len().to_string());
        out.push(':');
        out.push_str(frag);
    }
    out
}

/// Inverse of [`encode_group_key`], returning the tagged fragments in order.
/// `encode_group_key` is the only writer, so a malformed key (shouldn't happen)
/// degrades to what parsed rather than panicking.
pub fn decode_group_key(key: &str) -> Vec<String> {
    let bytes = key.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start || i >= bytes.len() || bytes[i] != b':' {
            break;
        }
        let Ok(len) = key[start..i].parse::<usize>() else {
            break;
        };
        i += 1; // skip ':'
        if i + len > bytes.len() {
            break;
        }
        out.push(key[i..i + len].to_string());
        i += len;
    }
    out
}

/// Reconstruct the display `Value` from a tagged group-key fragment (produced by
/// [`value_to_group_key_fragment`]). Lets the ghost grouped result show the
/// group value with its ORIGINAL type instead of stringifying everything.
pub fn group_key_fragment_to_value(frag: &str) -> Value {
    let mut chars = frag.chars();
    match chars.next() {
        Some('n') => Value::Null,
        Some('s') => Value::Text(frag[1..].to_string()),
        Some('i') => frag[1..]
            .parse::<i64>()
            .map(Value::Int)
            .unwrap_or(Value::Null),
        Some('f') => frag[1..]
            .parse::<f64>()
            .map(Value::Float)
            .unwrap_or(Value::Null),
        _ => Value::Text(frag.to_string()),
    }
}

/// Render a single value as its fragment inside a `GroupKey`. The
/// `GroupKey` for a record is `group_fields.map(this).join("|")`.
///
/// Exposed so that code needing to match against existing
/// `group_summaries` keys (e.g. `GhostLobeManager::read_precomputed`
/// when filtering by Eq predicates on group-key fields — Finding 11)
/// can produce the same fragment without reimplementing the
/// stringification. Single source of truth for the encoding.
pub fn value_to_group_key_fragment(value: Option<&Value>) -> String {
    // Leading type tag so a `Null` group value never collides with the text
    // `"null"`, nor `Int(5)` with the text `"5"`. Combined with the
    // length-prefix in `encode_group_key`, the group key is unambiguous.
    match value {
        Some(Value::Text(s)) => format!("s{s}"),
        Some(Value::Int(i)) => format!("i{i}"),
        Some(Value::Float(f)) => format!("f{f}"),
        Some(Value::Null) | None => "n".to_string(),
        Some(v) => format!("?{v:?}"),
    }
}

// ─── Rollup delta (hilo B: blind delta-append for ghost rollups) ──────────

/// Format byte for a postcard-encoded [`RollupDelta`] rollup value (hilo B),
/// behind `XYZDB_MAGIC`. The pre-hilo-B canonical `AggregateState` value used
/// `0x01`; a GHOST_META bump forces a rebuild so the two never coexist, but
/// [`decode_rollup_delta`] still folds a stray `0x01` in (as a positive delta).
const ROLLUP_DELTA_FORMAT: u8 = 0x02;
const ROLLUP_AGGSTATE_FORMAT: u8 = 0x01;

/// A signed contribution to a group's rollup. Written as a blind append (no
/// read-modify-write) and folded — at compaction and read — by the rollup
/// merge operator. Signed (`i64` count) so an add (`+1`) and a delete (`-1`)
/// are uniform, commutative deltas: the fold is order-independent, lock-free,
/// and needs no prior read. Min/Max cannot be decremented from a delta, so a
/// subtract sets `dirty` (reconciled by a full REFRESH), exactly as
/// [`AggregateState::subtract`] does.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RollupDelta {
    pub count: i64,
    pub values: BTreeMap<String, DeltaValue>,
    pub dirty: bool,
}

/// A signed per-metric aggregate contribution. Keys mirror `AggregateState`'s
/// metric labels so [`RollupDelta::into_aggregate_state`] yields the same map.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum DeltaValue {
    Count(i64),
    Sum(SumAcc),
    Min(StoredValue),
    Max(StoredValue),
    Avg { sum: SumAcc, count: i64 },
}

impl RollupDelta {
    /// Build the delta for one record. `sign` is `+1` for an add (insert /
    /// build) or `-1` for a subtract (delete / the old side of an update). For
    /// Min/Max a subtract contributes only `dirty` (a null candidate is neutral
    /// in the fold); an add contributes the value as a candidate.
    pub fn for_record(record: &Record, metrics: &[Metric], sign: i64) -> Self {
        let mut d = RollupDelta {
            count: sign,
            values: BTreeMap::new(),
            dirty: false,
        };
        for m in metrics {
            if m.rides_total_count() || !m.matches(record) {
                continue;
            }
            if let AggOp::Count = m.op {
                d.values.insert(m.label.clone(), DeltaValue::Count(sign));
                continue;
            }
            let Some(value) = record.fields.get(&m.field) else {
                continue;
            };
            let dv = match m.op {
                AggOp::Count => unreachable!("count handled above"),
                AggOp::Sum => {
                    let mut acc = SumAcc::default();
                    acc.add_scaled(value, sign);
                    DeltaValue::Sum(acc)
                }
                AggOp::Min => {
                    if sign < 0 {
                        d.dirty = true;
                        DeltaValue::Min(StoredValue::Null)
                    } else {
                        DeltaValue::Min(StoredValue::from_value(value))
                    }
                }
                AggOp::Max => {
                    if sign < 0 {
                        d.dirty = true;
                        DeltaValue::Max(StoredValue::Null)
                    } else {
                        DeltaValue::Max(StoredValue::from_value(value))
                    }
                }
                AggOp::Avg => {
                    let mut acc = SumAcc::default();
                    acc.add_scaled(value, sign);
                    DeltaValue::Avg {
                        sum: acc,
                        count: sign,
                    }
                }
            };
            d.values.insert(m.label.clone(), dv);
        }
        d
    }

    /// Fold another delta in. Associative + commutative: counts and sums add,
    /// min/max take the extreme of non-null candidates, dirty ORs.
    pub fn merge(&mut self, other: &RollupDelta) {
        self.count += other.count;
        self.dirty |= other.dirty;
        for (key, theirs) in &other.values {
            match self.values.entry(key.clone()) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(theirs.clone());
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    e.get_mut().merge(theirs);
                }
            }
        }
    }

    /// Build a positive delta equivalent to an existing [`AggregateState`].
    /// Used by the build/spill path (which accumulates in RAM as an
    /// `AggregateState`, then appends it as one delta) and to fold a legacy
    /// canonical value in.
    pub fn from_aggregate_state(st: &AggregateState) -> Self {
        let mut values = BTreeMap::new();
        for (k, av) in &st.values {
            let dv = match av {
                AggValue::Count(c) => DeltaValue::Count(*c as i64),
                AggValue::Sum(s) => DeltaValue::Sum(s.clone()),
                AggValue::Min(v) => DeltaValue::Min(v.clone()),
                AggValue::Max(v) => DeltaValue::Max(v.clone()),
                AggValue::Avg { sum, count } => DeltaValue::Avg {
                    sum: sum.clone(),
                    count: *count as i64,
                },
            };
            values.insert(k.clone(), dv);
        }
        RollupDelta {
            count: st.count as i64,
            values,
            dirty: st.dirty,
        }
    }

    /// Convert the folded delta to the query-facing [`AggregateState`]. Counts
    /// clamp at 0 (a fully-folded group is non-negative); `dirty` carries
    /// through for Min/Max reconciliation.
    pub fn into_aggregate_state(self) -> AggregateState {
        let mut values = BTreeMap::new();
        for (k, dv) in self.values {
            let av = match dv {
                DeltaValue::Count(c) => AggValue::Count(c.max(0) as u64),
                DeltaValue::Sum(s) => AggValue::Sum(s),
                DeltaValue::Min(v) => AggValue::Min(v),
                DeltaValue::Max(v) => AggValue::Max(v),
                DeltaValue::Avg { sum, count } => AggValue::Avg {
                    sum,
                    count: count.max(0) as u64,
                },
            };
            values.insert(k, av);
        }
        AggregateState {
            count: self.count.max(0) as u64,
            values,
            dirty: self.dirty,
        }
    }

    /// Encode as `[XYZDB_MAGIC][0x02][postcard(RollupDelta)]`.
    pub fn encode(&self) -> Vec<u8> {
        let payload = postcard::to_allocvec(self).unwrap_or_default();
        let mut bytes = Vec::with_capacity(3 + payload.len());
        bytes.extend_from_slice(&xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(ROLLUP_DELTA_FORMAT);
        bytes.extend_from_slice(&payload);
        bytes
    }
}

impl DeltaValue {
    fn merge(&mut self, other: &DeltaValue) {
        match (self, other) {
            (DeltaValue::Count(a), DeltaValue::Count(b)) => *a += b,
            (DeltaValue::Sum(a), DeltaValue::Sum(b)) => a.merge(b),
            (DeltaValue::Min(a), DeltaValue::Min(b)) => {
                if *b != StoredValue::Null && (*a == StoredValue::Null || *b < *a) {
                    *a = b.clone();
                }
            }
            (DeltaValue::Max(a), DeltaValue::Max(b)) => {
                if *b != StoredValue::Null && (*a == StoredValue::Null || *b > *a) {
                    *a = b.clone();
                }
            }
            (DeltaValue::Avg { sum: s, count: c }, DeltaValue::Avg { sum: os, count: oc }) => {
                s.merge(os);
                *c += oc;
            }
            _ => {}
        }
    }
}

/// Decode a rollup value into a [`RollupDelta`]. Accepts the hilo-B delta format
/// (`0x02`) and folds a legacy canonical `AggregateState` (`0x01`) in as a
/// positive delta. Returns `None` on a bad magic / unknown format.
pub fn decode_rollup_delta(bytes: &[u8]) -> Option<RollupDelta> {
    if bytes.len() < 3 || bytes[0..2] != xyzdb_core::record::XYZDB_MAGIC {
        return None;
    }
    match bytes[2] {
        ROLLUP_DELTA_FORMAT => postcard::from_bytes(&bytes[3..]).ok(),
        ROLLUP_AGGSTATE_FORMAT => {
            // Legacy canonical AggregateState → equivalent positive delta.
            let st: AggregateState = postcard::from_bytes(&bytes[3..]).ok()?;
            Some(RollupDelta::from_aggregate_state(&st))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical five metrics over `field`: a fieldless `count()` (rides the
    /// group total) plus sum/min/max/avg keyed by the canonical `field:Op` label.
    fn specs(field: &str) -> Vec<Metric> {
        vec![
            Metric::new(String::new(), AggOp::Count, COUNT_LABEL.to_string(), None),
            Metric::new(field.into(), AggOp::Sum, format!("{field}:Sum"), None),
            Metric::new(field.into(), AggOp::Min, format!("{field}:Min"), None),
            Metric::new(field.into(), AggOp::Max, format!("{field}:Max"), None),
            Metric::new(field.into(), AggOp::Avg, format!("{field}:Avg"), None),
        ]
    }

    fn fields(k: &str, v: f64) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(k.to_string(), Value::Float(v));
        m
    }

    fn int_fields(k: &str, v: i64) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(k.to_string(), Value::Int(v));
        m
    }

    /// Wrap a fields map in a minimal `Record` for the add/subtract path.
    fn rec(fields: BTreeMap<String, Value>) -> Record {
        Record {
            lid: xyzdb_core::lid::LID::new(1),
            lobe_name: String::new(),
            fields,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn sum_of(st: &AggregateState) -> f64 {
        match st.values.get("monto:Sum") {
            Some(AggValue::Sum(s)) => s.to_f64(),
            _ => panic!("no Sum"),
        }
    }

    #[test]
    fn sum_integer_composition_exact_via_delta_path() {
        // Money as integer cents. The delta path (add then subtract the same
        // large value with a small one in between) must net EXACTLY; the naive
        // `f64 += / -=` composes rounding error and loses the small addend.
        let s = specs("monto");
        let big = 9_007_199_254_740_992_i64; // 2^53 — the last f64-exact integer
        let mut st = AggregateState::default();
        st.add(&rec(int_fields("monto", big)), &s);
        st.add(&rec(int_fields("monto", 1)), &s); // 2^53 + 1 — not f64-representable
        st.subtract(&rec(int_fields("monto", big)), &s); // nets back to 1
        // Exact integer arithmetic: 1. Naive f64 rounds 2^53+1 → 2^53 at the
        // middle add, so the subtract lands on 0.0 (the +1 is gone).
        assert_eq!(sum_of(&st), 1.0);
    }

    #[test]
    fn sum_float_compensated_recovers_small_addends() {
        // A large float swamps small ones under naive summation. Neumaier
        // compensation keeps them; the delta path then nets exactly.
        let s = specs("monto");
        let mut st = AggregateState::default();
        st.add(&rec(fields("monto", 1e16)), &s);
        for _ in 0..100 {
            st.add(&rec(fields("monto", 1.0)), &s); // each 1.0 is lost next to 1e16
        }
        st.subtract(&rec(fields("monto", 1e16)), &s);
        // True sum = 100.0. Naive f64 loses every 1.0 → 0.0.
        assert_eq!(sum_of(&st), 100.0);
    }

    #[test]
    fn rollup_delta_add_fold_matches_direct_aggregate() {
        let s = specs("monto");
        // Fold three add-deltas in arbitrary grouping (associativity).
        let mut a = RollupDelta::for_record(&rec(fields("monto", 10.0)), &s, 1);
        let mut b = RollupDelta::for_record(&rec(fields("monto", 20.0)), &s, 1);
        b.merge(&RollupDelta::for_record(&rec(fields("monto", 30.0)), &s, 1));
        a.merge(&b);
        let st = a.into_aggregate_state();

        // Direct aggregate over the same records.
        let mut direct = AggregateState::default();
        for v in [10.0, 20.0, 30.0] {
            direct.add(&rec(fields("monto", v)), &s);
        }
        assert_eq!(st.count, direct.count);
        assert!((sum_of(&st) - sum_of(&direct)).abs() < 1e-9);
        assert!((sum_of(&st) - 60.0).abs() < 1e-9);
        assert!(!st.dirty);
    }

    #[test]
    fn rollup_delta_add_minus_subtract_nets() {
        let s = specs("monto");
        let mut d = RollupDelta::for_record(&rec(fields("monto", 10.0)), &s, 1);
        d.merge(&RollupDelta::for_record(&rec(fields("monto", 20.0)), &s, 1));
        d.merge(&RollupDelta::for_record(
            &rec(fields("monto", 10.0)),
            &s,
            -1,
        )); // delete one
        let st = d.into_aggregate_state();
        assert_eq!(st.count, 1); // 2 adds − 1 delete
        assert!((sum_of(&st) - 20.0).abs() < 1e-9); // 10 + 20 − 10
        assert!(st.dirty, "a subtract must mark Min/Max dirty");
    }

    #[test]
    fn rollup_delta_count_clamps_at_zero() {
        let s = specs("monto");
        let mut d = RollupDelta::for_record(&rec(fields("monto", 5.0)), &s, 1);
        d.merge(&RollupDelta::for_record(&rec(fields("monto", 5.0)), &s, -1));
        assert_eq!(d.into_aggregate_state().count, 0);
    }

    #[test]
    fn rollup_delta_encode_decode_roundtrip() {
        let d = RollupDelta::for_record(&rec(fields("monto", 5.0)), &specs("monto"), 1);
        assert_eq!(decode_rollup_delta(&d.encode()), Some(d));
        assert_eq!(decode_rollup_delta(b"\x00\x00\x02junk"), None); // bad magic
    }

    #[test]
    fn rollup_delta_folds_in_legacy_aggregate_state() {
        let mut st = AggregateState::default();
        st.add(&rec(fields("monto", 7.0)), &specs("monto"));
        let mut bytes = Vec::from(xyzdb_core::record::XYZDB_MAGIC);
        bytes.push(0x01); // legacy canonical AggregateState format
        bytes.extend_from_slice(&postcard::to_allocvec(&st).unwrap());
        let d = decode_rollup_delta(&bytes).expect("legacy 0x01 decodes");
        assert_eq!(d.count, 1);
        assert!((sum_of(&d.into_aggregate_state()) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_add_count_and_sum() {
        let mut state = AggregateState::default();
        let s = specs("monto");
        state.add(&rec(fields("monto", 100.0)), &s);
        state.add(&rec(fields("monto", 200.0)), &s);
        state.add(&rec(fields("monto", 300.0)), &s);

        // count() rides the group total (self.count), not a per-field entry.
        assert_eq!(state.count, 3);
        match state.values.get("monto:Sum") {
            Some(AggValue::Sum(v)) => assert!((v.to_f64() - 600.0).abs() < 0.001),
            _ => panic!("expected Sum"),
        }
    }

    #[test]
    fn aggregate_subtract_sum_and_count() {
        let mut state = AggregateState::default();
        let s = specs("monto");
        state.add(&rec(fields("monto", 100.0)), &s);
        state.add(&rec(fields("monto", 200.0)), &s);
        state.subtract(&rec(fields("monto", 100.0)), &s);

        assert_eq!(state.count, 1);
        match state.values.get("monto:Sum") {
            Some(AggValue::Sum(v)) => assert!((v.to_f64() - 200.0).abs() < 0.001),
            _ => panic!("expected Sum"),
        }
    }

    #[test]
    fn aggregate_avg_incremental() {
        let mut state = AggregateState::default();
        let s = specs("monto");
        state.add(&rec(fields("monto", 10.0)), &s);
        state.add(&rec(fields("monto", 20.0)), &s);
        state.add(&rec(fields("monto", 30.0)), &s);

        match state.values.get("monto:Avg") {
            Some(AggValue::Avg { sum, count }) => {
                assert_eq!(*count, 3);
                assert!((sum.to_f64() / *count as f64 - 20.0).abs() < 0.001);
            }
            _ => panic!("expected Avg"),
        }
    }

    #[test]
    fn avg_numerator_exact_via_delta_path() {
        // Avg's numerator is a SumAcc too, so the delta path is exact where the
        // old naive f64 numerator composed error. add(2^53), add(1) [count 2],
        // subtract(2^53) [count 1] → numerator = 1, avg = 1/1 = 1.0. Naive f64
        // loses the +1 at the middle add → avg 0.0.
        let s = specs("monto");
        let big = 9_007_199_254_740_992_i64; // 2^53
        let mut st = AggregateState::default();
        st.add(&rec(int_fields("monto", big)), &s);
        st.add(&rec(int_fields("monto", 1)), &s);
        st.subtract(&rec(int_fields("monto", big)), &s);
        match st.values.get("monto:Avg") {
            Some(AggValue::Avg { sum, count }) => {
                assert_eq!(*count, 1);
                assert_eq!(sum.to_f64() / *count as f64, 1.0);
            }
            _ => panic!("expected Avg"),
        }
    }

    #[test]
    fn aggregate_min_max() {
        let mut state = AggregateState::default();
        let s = specs("monto");
        state.add(&rec(fields("monto", 50.0)), &s);
        state.add(&rec(fields("monto", 10.0)), &s);
        state.add(&rec(fields("monto", 90.0)), &s);

        match state.values.get("monto:Min") {
            Some(AggValue::Min(StoredValue::Float(v))) => assert!((v - 10.0).abs() < 0.001),
            _ => panic!("expected Min"),
        }
        match state.values.get("monto:Max") {
            Some(AggValue::Max(StoredValue::Float(v))) => assert!((v - 90.0).abs() < 0.001),
            _ => panic!("expected Max"),
        }
    }

    #[test]
    fn aggregate_subtract_min_marks_dirty() {
        let mut state = AggregateState::default();
        let s = specs("monto");
        state.add(&rec(fields("monto", 10.0)), &s);
        state.add(&rec(fields("monto", 20.0)), &s);
        assert!(!state.dirty);

        state.subtract(&rec(fields("monto", 10.0)), &s);
        assert!(state.dirty, "subtracting should mark min/max dirty");
    }

    #[test]
    fn aggregate_empty_to_result() {
        let state = AggregateState::default();
        let result = state.to_result();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "count");
        assert_eq!(result[0].1, Value::Int(0));
    }

    #[test]
    fn aggregate_reset() {
        let mut state = AggregateState::default();
        let s = specs("monto");
        state.add(&rec(fields("monto", 100.0)), &s);
        state.dirty = true;
        state.reset();
        assert_eq!(state.count, 0);
        assert!(state.values.is_empty());
        assert!(!state.dirty);
    }

    #[test]
    fn group_key_extraction() {
        let mut f = BTreeMap::new();
        f.insert("status".to_string(), Value::Text("active".into()));
        f.insert("tipo".to_string(), Value::Text("personal".into()));

        let gk = extract_group_key(&f, &["status".to_string(), "tipo".to_string()]);
        // Length-prefixed + type-tagged: decode round-trips to the typed values.
        let parts = decode_group_key(&gk);
        assert_eq!(parts.len(), 2);
        assert_eq!(
            group_key_fragment_to_value(&parts[0]),
            Value::Text("active".into())
        );
        assert_eq!(
            group_key_fragment_to_value(&parts[1]),
            Value::Text("personal".into())
        );
    }

    #[test]
    fn group_summary_insert_and_move() {
        let s = specs("monto");
        let mut groups: BTreeMap<GroupKey, AggregateState> = BTreeMap::new();

        // Insert into "active" group
        let f1 = {
            let mut m = fields("monto", 100.0);
            m.insert("status".to_string(), Value::Text("active".into()));
            m
        };
        let active_key = extract_group_key(&f1, &["status".to_string()]);
        let r1 = rec(f1.clone());
        groups.entry(active_key.clone()).or_default().add(&r1, &s);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[&active_key].count, 1);

        // Move to "paid" group (subtract from active, add to paid)
        let f2 = {
            let mut m = fields("monto", 100.0);
            m.insert("status".to_string(), Value::Text("paid".into()));
            m
        };
        groups.get_mut(&active_key).unwrap().subtract(&r1, &s);
        if groups[&active_key].count == 0 {
            groups.remove(&active_key);
        }
        let paid_key = extract_group_key(&f2, &["status".to_string()]);
        groups
            .entry(paid_key.clone())
            .or_default()
            .add(&rec(f2.clone()), &s);

        assert!(
            !groups.contains_key(&active_key),
            "empty group should be removed"
        );
        assert_eq!(groups[&paid_key].count, 1);
    }

    #[test]
    fn subtract_count_below_zero_saturates() {
        let mut state = AggregateState::default();
        let s = vec![Metric::new(
            String::new(),
            AggOp::Count,
            COUNT_LABEL.to_string(),
            None,
        )];
        state.subtract(&rec(fields("x", 1.0)), &s);
        assert_eq!(state.count, 0, "should not underflow");
    }

    #[test]
    fn group_key_length_prefixed_kills_collisions() {
        // Separator collision: one field "a|b" must NOT equal two fields "a","b".
        // The old '|'-join produced "a|b" for both.
        let one = encode_group_key(&[value_to_group_key_fragment(Some(&Value::Text(
            "a|b".into(),
        )))]);
        let two = encode_group_key(&[
            value_to_group_key_fragment(Some(&Value::Text("a".into()))),
            value_to_group_key_fragment(Some(&Value::Text("b".into()))),
        ]);
        assert_ne!(
            one, two,
            "'|' in a value must not merge with a two-field key"
        );

        // Type collisions: Null vs "null", Int(5) vs "5" — the old encoding
        // stringified both sides to the same fragment.
        let null_v = encode_group_key(&[value_to_group_key_fragment(Some(&Value::Null))]);
        let null_s = encode_group_key(&[value_to_group_key_fragment(Some(&Value::Text(
            "null".into(),
        )))]);
        assert_ne!(
            null_v, null_s,
            "Null must not collide with the text \"null\""
        );
        let int_v = encode_group_key(&[value_to_group_key_fragment(Some(&Value::Int(5)))]);
        let int_s =
            encode_group_key(&[value_to_group_key_fragment(Some(&Value::Text("5".into())))]);
        assert_ne!(int_v, int_s, "Int(5) must not collide with the text \"5\"");

        // Round-trip with an adversarial value (contains '|' and ':').
        let frags = vec![
            value_to_group_key_fragment(Some(&Value::Int(42))),
            value_to_group_key_fragment(Some(&Value::Text("x|y:z".into()))),
            value_to_group_key_fragment(Some(&Value::Null)),
        ];
        let decoded = decode_group_key(&encode_group_key(&frags));
        assert_eq!(decoded, frags, "decode must invert encode");
        assert_eq!(group_key_fragment_to_value(&decoded[0]), Value::Int(42));
        assert_eq!(
            group_key_fragment_to_value(&decoded[1]),
            Value::Text("x|y:z".into())
        );
        assert_eq!(group_key_fragment_to_value(&decoded[2]), Value::Null);
    }
}
