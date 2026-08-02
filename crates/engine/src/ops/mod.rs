pub mod aggregate;
pub mod delete;
pub mod fetch;
pub mod find;
pub mod follow;
pub mod link;
pub mod nearest;
pub mod pull;
pub mod put;
pub mod scan;
pub mod set;

use xytalk_parser::ast;
use xyzdb_core::record::FilterOp;
use xyzdb_core::value::Value;

/// Minimum length for a homogeneous float list to be stored as a packed `Value::Vector(f32)`.
///
/// Lists at or above this length are treated as embeddings and packed to f32 (~2x denser
/// on disk and in RAM than `List(Float(f64))`). Shorter float lists stay `List(Float(f64))`
/// so small precision-sensitive lists (geo coordinates, prices) keep full f64 precision.
/// 64 sits well below every real embedding width (MiniLM 384, nomic 768, larger models 1536) and
/// well above any common short float list, so misclassification is by size only — never a
/// correctness change for embeddings, which are the target.
pub const VECTOR_F32_MIN_DIMS: usize = 64;

/// Convert a parser Literal to a core Value.
pub(crate) fn literal_to_value(lit: &ast::Literal) -> Value {
    match lit {
        ast::Literal::Int(v) => Value::Int(*v),
        ast::Literal::Float(v) => Value::Float(*v),
        ast::Literal::Text(v) => Value::Text(v.clone()),
        ast::Literal::Bool(v) => Value::Bool(*v),
        ast::Literal::Timestamp(s) => {
            // Simple ISO 8601 to micros: parse date, optionally time
            Value::Timestamp(parse_timestamp_micros(s))
        }
        ast::Literal::Lid(s) => Value::Text(s.clone()), // LIDs stored as text in fields
        ast::Literal::Null => Value::Null,
        ast::Literal::List(items) => list_literal_to_value(items),
        ast::Literal::Map(pairs) => Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), literal_to_value(v)))
                .collect(),
        ),
        // S1: bind_params substitutes every Param before execution and rejects
        // unbound ones, so a Param here is an internal invariant violation.
        ast::Literal::Param(name) => {
            unreachable!("unbound parameter ${name} reached executor (bind_params skipped?)")
        }
    }
}

/// Convert a list literal, packing it into a dense `Value::Vector(f32)` when it looks like
/// an embedding: every element is a `Float` and the length is `>= VECTOR_F32_MIN_DIMS`.
/// Otherwise it stays a `Value::List` (preserving int/mixed lists and short f64 lists).
fn list_literal_to_value(items: &[ast::Literal]) -> Value {
    let all_float = items.iter().all(|it| matches!(it, ast::Literal::Float(_)));
    if all_float && items.len() >= VECTOR_F32_MIN_DIMS {
        let packed: Vec<f32> = items
            .iter()
            .map(|it| match it {
                ast::Literal::Float(f) => *f as f32,
                _ => unreachable!("all_float checked above"),
            })
            .collect();
        Value::Vector(packed)
    } else {
        Value::List(items.iter().map(literal_to_value).collect())
    }
}

/// Convert parser FilterOp to core FilterOp.
pub(crate) fn convert_filter_op(op: &ast::FilterOp) -> FilterOp {
    match op {
        ast::FilterOp::Eq => FilterOp::Eq,
        ast::FilterOp::Neq => FilterOp::Neq,
        ast::FilterOp::Gt => FilterOp::Gt,
        ast::FilterOp::Gte => FilterOp::Gte,
        ast::FilterOp::Lt => FilterOp::Lt,
        ast::FilterOp::Lte => FilterOp::Lte,
        ast::FilterOp::IsNull => FilterOp::IsNull,
        ast::FilterOp::IsNotNull => FilterOp::IsNotNull,
        ast::FilterOp::Contains => FilterOp::Contains,
        ast::FilterOp::In => FilterOp::In,
    }
}

/// Convert AST filters to (field, op, value) tuples for Record::matches_filters.
pub(crate) fn convert_filters(filters: &[ast::Filter]) -> Vec<(String, FilterOp, Value)> {
    filters
        .iter()
        .map(|f| {
            (
                f.field.clone(),
                convert_filter_op(&f.op),
                literal_to_value(&f.value),
            )
        })
        .collect()
}

/// Extract text from a Literal (for anchor value matching).
pub(crate) fn literal_to_string(lit: &ast::Literal) -> String {
    match lit {
        ast::Literal::Int(v) => v.to_string(),
        ast::Literal::Float(v) => v.to_string(),
        ast::Literal::Text(v) => v.clone(),
        ast::Literal::Bool(v) => v.to_string(),
        ast::Literal::Timestamp(v) => v.clone(),
        ast::Literal::Lid(v) => v.clone(),
        ast::Literal::Null => "null".to_string(),
        ast::Literal::List(items) => format!("{items:?}"),
        ast::Literal::Map(pairs) => format!("{pairs:?}"),
        ast::Literal::Param(name) => {
            unreachable!("unbound parameter ${name} reached executor (bind_params skipped?)")
        }
    }
}

/// Convert an optional FilterExpr to flat AND filters (for backward compat paths).
/// Returns empty vec if None or if the expression is pure AND.
pub(crate) fn filter_expr_to_flat(
    expr: &Option<xytalk_parser::ast::FilterExpr>,
) -> Vec<xytalk_parser::ast::Filter> {
    match expr {
        None => vec![],
        Some(e) => match e.as_flat_and() {
            Some(filters) => filters.into_iter().cloned().collect(),
            None => vec![], // OR/NOT expressions can't be flattened — return empty (primary scan)
        },
    }
}

/// Convert an optional FilterExpr to core filter tuples for Record::matches_filters().
pub(crate) fn convert_filter_expr(
    expr: &Option<xytalk_parser::ast::FilterExpr>,
) -> Vec<(String, FilterOp, Value)> {
    convert_filters(&filter_expr_to_flat(expr))
}

/// A boolean filter tree in the CORE representation: leaves carry already-
/// converted `(field, FilterOp, Value)` tuples, not parser AST. This is the
/// single evaluated form — the ghost caches it once (so the write path never
/// reconverts AST→core, preserving audit P2-2) and SCAN builds it from its AST
/// `FilterExpr` via [`to_core_expr`]. Both then walk it with the one evaluator
/// [`matches_core_expr`]. A flat `Vec<Filter>` is just an `And` of `Leaf`s.
#[derive(Clone, Debug)]
pub(crate) enum CoreFilterExpr {
    Leaf((String, FilterOp, Value)),
    And(Vec<CoreFilterExpr>),
    Or(Vec<CoreFilterExpr>),
    Not(Box<CoreFilterExpr>),
}

impl CoreFilterExpr {
    /// Build an `And` tree from already-converted core predicates. Empty list →
    /// `And([])`, which evaluates to `true`, exactly like
    /// `matches_filters(&[]).all()`. Test-only since 2.3 (production builds the
    /// tree from the ghost's `FilterExpr` via [`to_core_expr`]); kept because
    /// the And-tree ≡ flat-`.all()` equivalence test reads clearest through it.
    #[cfg(test)]
    pub(crate) fn and_of(leaves: Vec<(String, FilterOp, Value)>) -> Self {
        CoreFilterExpr::And(leaves.into_iter().map(CoreFilterExpr::Leaf).collect())
    }
}

/// Convert a parser `FilterExpr` (AST) to the core-typed tree once. Callers on
/// a hot per-record path must convert ONCE (outside the loop) and then walk the
/// result, never per record.
pub(crate) fn to_core_expr(expr: &xytalk_parser::ast::FilterExpr) -> CoreFilterExpr {
    use xytalk_parser::ast::FilterExpr;
    match expr {
        FilterExpr::Condition(f) => CoreFilterExpr::Leaf((
            f.field.clone(),
            convert_filter_op(&f.op),
            literal_to_value(&f.value),
        )),
        FilterExpr::And(exprs) => CoreFilterExpr::And(exprs.iter().map(to_core_expr).collect()),
        FilterExpr::Or(exprs) => CoreFilterExpr::Or(exprs.iter().map(to_core_expr).collect()),
        FilterExpr::Not(e) => CoreFilterExpr::Not(Box::new(to_core_expr(e))),
    }
}

/// The single filter evaluator. Walks a [`CoreFilterExpr`]; each leaf delegates
/// to `Record::matches_filters` (the shared per-predicate primitive) via a
/// borrow — no per-record allocation or reconversion.
pub(crate) fn matches_core_expr(
    record: &xyzdb_core::record::Record,
    expr: &CoreFilterExpr,
) -> bool {
    match expr {
        CoreFilterExpr::Leaf(t) => record.matches_filters(std::slice::from_ref(t)),
        CoreFilterExpr::And(exprs) => exprs.iter().all(|e| matches_core_expr(record, e)),
        CoreFilterExpr::Or(exprs) => exprs.iter().any(|e| matches_core_expr(record, e)),
        CoreFilterExpr::Not(e) => !matches_core_expr(record, e),
    }
}

/// Evaluate a parser `FilterExpr` against a record. Adapter over the single
/// evaluator: converts to the core tree, then walks it. Callers on a hot loop
/// should prefer converting once with [`to_core_expr`] and calling
/// [`matches_core_expr`] directly (this converts on every call).
pub(crate) fn matches_filter_expr(
    record: &xyzdb_core::record::Record,
    expr: &xytalk_parser::ast::FilterExpr,
) -> bool {
    matches_core_expr(record, &to_core_expr(expr))
}

/// Check if a record matches an optional FilterExpr (None = no filter = always matches).
pub(crate) fn record_matches_opt_expr(
    record: &xyzdb_core::record::Record,
    expr: &Option<xytalk_parser::ast::FilterExpr>,
) -> bool {
    match expr {
        None => true,
        Some(e) => matches_filter_expr(record, e),
    }
}

/// Deserialize a record blob and, for the V5 split layout, re-attach its search
/// vector from the `vectors` keyspace so the returned `Record` is identical to a
/// V1–V4 one.
///
/// V5 stores the searchable vector in a separate column keyed by the SAME
/// `spatial_key` as the blob; `deserialize_record` decodes a V5 blob WITHOUT
/// that field. This helper point-gets the column entry and hydrates it back, so
/// every read path that returns a record observes the vector exactly as before
/// the split. Blobs at V1–V4 (vector inline, or no vector) and a missing field
/// dict pass straight through `deserialize_record` with no extra lookup.
///
/// `spatial_key` is the record's spatial key (the column key). `fd` is the
/// lobe's field dict, required to resolve the column's field id to a name; a
/// `None` dict cannot hydrate, matching `deserialize_record`'s own contract.
///
/// # Errors
/// Propagates `deserialize_record`'s storage/decoding errors. A column fetch
/// that fails or is absent leaves the record un-hydrated rather than erroring —
/// the blob alone is still a valid (vector-less) record.
pub(crate) fn deserialize_hydrated(
    engine: &crate::engine::Engine,
    spatial_key: &[u8],
    blob: &[u8],
    lobe_name: &str,
    fd: Option<&xyzdb_core::field_dict::FieldDict>,
) -> xyzdb_core::error::Result<xyzdb_core::record::Record> {
    deserialize_hydrated_with(&engine.turba().vectors, spatial_key, blob, lobe_name, fd)
}

/// [`deserialize_hydrated`] against a `vectors` tree handle instead of the whole
/// engine — for read paths that hold trees rather than an `&Engine`, such as the
/// ghost point-read. Same contract; `deserialize_hydrated` delegates here so the
/// two cannot drift apart.
pub(crate) fn deserialize_hydrated_with(
    vectors: &turba_engine::tree::Tree,
    spatial_key: &[u8],
    blob: &[u8],
    lobe_name: &str,
    fd: Option<&xyzdb_core::field_dict::FieldDict>,
) -> xyzdb_core::error::Result<xyzdb_core::record::Record> {
    let mut record = xyzdb_core::record::deserialize_record(blob, lobe_name, fd)?;
    if xyzdb_core::record::format_version(blob) == 5
        && let Some(dict) = fd
        && let Ok(Some(column)) = vectors.get(spatial_key)
    {
        xyzdb_core::record::hydrate_vector(&mut record, &column, dict);
    }
    Ok(record)
}

// NOTE: the per-value gravity dictionary entry
// ([0xFE][lobe_id:2][field][0x00][value] → LID) was retired in 0.7.5.
// Gravity values are 1→N (a bucket, not an identity), so the single-LID
// entry truncated FIND results and leaked on DELETE. FIND now resolves
// gravity predicates via the bounded bucket range scan. Pre-0.7.5
// datasets may still hold inert 0xFE entries; nothing reads them.

fn parse_timestamp_micros(s: &str) -> i64 {
    // Simple parser: "YYYY-MM-DD" or "YYYY-MM-DDTHH:MM:SS"
    // Returns approximate microseconds since epoch.
    // Full chrono parsing deferred — this is sufficient for MVP.
    let parts: Vec<&str> = s.split(['-', 'T', ':']).collect();
    if parts.len() < 3 {
        return 0;
    }
    let year: i64 = parts[0].parse().unwrap_or(2026);
    let month: i64 = parts[1].parse().unwrap_or(1);
    let day: i64 = parts[2].parse().unwrap_or(1);
    let hour: i64 = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: i64 = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let sec: i64 = parts.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);

    // Approximate days since epoch (not accounting for leap years perfectly)
    let days = (year - 1970) * 365 + (year - 1969) / 4 + (month - 1) * 30 + day;
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    secs * 1_000_000
}

#[cfg(test)]
mod vector_packing_tests {
    use super::*;

    fn float_list(n: usize) -> ast::Literal {
        ast::Literal::List(
            (0..n)
                .map(|i| ast::Literal::Float(i as f64 * 0.01))
                .collect(),
        )
    }

    #[test]
    fn long_float_list_packs_to_vector_f32() {
        let v = literal_to_value(&float_list(VECTOR_F32_MIN_DIMS));
        match v {
            Value::Vector(p) => assert_eq!(p.len(), VECTOR_F32_MIN_DIMS),
            other => panic!("expected Vector, got {other:?}"),
        }
    }

    #[test]
    fn short_float_list_stays_list_f64() {
        let v = literal_to_value(&float_list(VECTOR_F32_MIN_DIMS - 1));
        assert!(
            matches!(v, Value::List(_)),
            "short float list must keep f64 List"
        );
    }

    #[test]
    fn int_and_mixed_lists_stay_list() {
        let ints = ast::Literal::List((0..128).map(ast::Literal::Int).collect());
        assert!(
            matches!(literal_to_value(&ints), Value::List(_)),
            "int list must stay List"
        );

        let mut items: Vec<ast::Literal> =
            (0..128).map(|i| ast::Literal::Float(i as f64)).collect();
        items[0] = ast::Literal::Int(0); // one non-Float element disqualifies packing
        assert!(matches!(
            literal_to_value(&ast::Literal::List(items)),
            Value::List(_)
        ));
    }
}

#[cfg(test)]
mod filter_evaluator_tests {
    use super::*;
    use xyzdb_core::record::Record;

    fn rec(pairs: &[(&str, Value)]) -> Record {
        let mut fields = std::collections::BTreeMap::new();
        for (k, v) in pairs {
            fields.insert(k.to_string(), v.clone());
        }
        Record {
            lid: xyzdb_core::lid::LID::from_raw(0),
            lobe_name: "l".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// The load-bearing equivalence of the evaluator unification: a flat-AND
    /// `Vec` under `matches_filters` (`.all()`) must equal the same leaves as an
    /// `And` tree under the single walker `matches_core_expr`, at every edge —
    /// empty (vacuous true), single, and multi with mixed types.
    #[test]
    fn and_tree_equals_flat_all_at_edges() {
        let r = rec(&[
            ("status", Value::Text("active".into())),
            ("age", Value::Int(30)),
        ]);

        // Empty: And([]) == matches_filters(&[]) == true (vacuous).
        let empty: Vec<(String, FilterOp, Value)> = vec![];
        assert_eq!(
            matches_core_expr(&r, &CoreFilterExpr::and_of(empty.clone())),
            r.matches_filters(&empty)
        );
        assert!(matches_core_expr(&r, &CoreFilterExpr::and_of(vec![])));

        // Single leaf: match and non-match agree with the flat form.
        for val in ["active", "inactive"] {
            let leaves = vec![("status".to_string(), FilterOp::Eq, Value::Text(val.into()))];
            assert_eq!(
                matches_core_expr(&r, &CoreFilterExpr::and_of(leaves.clone())),
                r.matches_filters(&leaves),
                "single-leaf And must equal flat .all() for status={val}"
            );
        }

        // Multiple leaves, mixed types (Text Eq + Int Gt) across all outcomes.
        for (s, age) in [("active", 40), ("active", 10), ("inactive", 40)] {
            let rr = rec(&[("status", Value::Text(s.into())), ("age", Value::Int(age))]);
            let leaves = vec![
                (
                    "status".to_string(),
                    FilterOp::Eq,
                    Value::Text("active".into()),
                ),
                ("age".to_string(), FilterOp::Gt, Value::Int(18)),
            ];
            assert_eq!(
                matches_core_expr(&rr, &CoreFilterExpr::and_of(leaves.clone())),
                rr.matches_filters(&leaves),
                "And-tree must equal flat .all() for status={s} age={age}"
            );
        }
    }

    /// Or/Not arms of the single walker (used by SCAN today; by ghosts once
    /// they can store non-flat filters).
    #[test]
    fn core_walker_or_and_not() {
        let r = rec(&[("status", Value::Text("active".into()))]);
        let leaf = |v: &str| {
            CoreFilterExpr::Leaf(("status".to_string(), FilterOp::Eq, Value::Text(v.into())))
        };
        assert!(matches_core_expr(
            &r,
            &CoreFilterExpr::Or(vec![leaf("x"), leaf("active")])
        ));
        assert!(!matches_core_expr(
            &r,
            &CoreFilterExpr::Or(vec![leaf("x"), leaf("y")])
        ));
        assert!(matches_core_expr(
            &r,
            &CoreFilterExpr::Not(Box::new(leaf("x")))
        ));
        assert!(!matches_core_expr(
            &r,
            &CoreFilterExpr::Not(Box::new(leaf("active")))
        ));
    }
}
