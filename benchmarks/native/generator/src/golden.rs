//! v0.3.4 Phase E — verify-golden file format.
//!
//! Truth source: generator iterators in-memory
//! (`Dataset::compute_golden_aggregates`), NOT any engine's `bulk_load`
//! result. See the cross-engine bench design notes §12.3 entry "Verify-golden
//! methodology" + caveat C-8.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::ExpectedCounts;

/// Top-level golden manifest. Header carries the run-context fields that
/// future readers need to interpret the verify_queries values; payload
/// is the V1-V6 set defined in the cross-engine bench design notes §12.3.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenFile {
    /// Cycle version that authored this golden. Bump when V-set changes.
    pub version: String,
    pub seed: u64,
    pub scale: f64,
    /// ISO-8601 wall-clock at generation. Currently informational; turns
    /// load-bearing the moment a V-query depends on time-relative data
    /// (caveat C-8: generator deterministic by seed+scale but NOT by
    /// wall-clock; reference_now lets a future consumer reconstruct the
    /// dataset's time anchor without guessing).
    pub reference_now: String,
    /// Tolerance applied by `verify_golden` when comparing observed
    /// engine values vs golden. Counts are exact; sums use this relative
    /// tolerance. Stored in the file (not just hardcoded in orchestrator)
    /// so the golden auto-documents the comparison semantic.
    pub tolerance_f64_relative: f64,
    /// Caveats explicitly excluded from this golden's V-query set. A
    /// future agent comparing two goldens spots regime drift via the
    /// diff of this list. The former standing exclusion (C-3, Surreal Q8
    /// 3-step asymmetry) was retired with the SurrealDB driver; the list
    /// is empty for the current three-engine set unless a new caveat is added.
    pub caveats_active: Vec<String>,
    pub verify_queries: GoldenVerifyQueries,
}

/// V-query payload. Each entry is the deterministic answer that every
/// engine should return for the equivalent business question over the
/// (seed, scale) dataset.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenVerifyQueries {
    /// V1 — `creditos` `_type=Credit`: count + sum(monto).
    pub v1_credits_total: AggregateCountSum,
    /// V2 — `installments` WHERE status="overdue": count + sum(monto_total).
    pub v2_installments_overdue: AggregateCountSum,
    /// V3 — `payments`: count + sum(monto).
    pub v3_payments_total: AggregateCountSum,
    /// V4 — counts by (lobe, _type). The most fine-grained verify; if any
    /// lobe×type cardinality differs the engine is missing or duplicating
    /// a record class.
    pub v4_lobe_type_counts: V4LobeTypeCounts,
    /// V5 — distinct rfc on `clients`. Cardinality canary.
    pub v5_clients_distinct_rfc: AggregateCount,
    /// V6 — `configuracion` catalogue counts. Smallest lobe; cheapest canary.
    pub v6_config_counts: V6ConfigCounts,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AggregateCountSum {
    pub n: u64,
    pub sum: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AggregateCount {
    pub n: u64,
}

/// Lobe × _type counts. BTreeMap so JSON output is stable-ordered (diffs
/// across runs only show real changes, not key-reorder noise).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V4LobeTypeCounts {
    pub clientes: BTreeMap<String, u64>,
    pub creditos: BTreeMap<String, u64>,
    pub operaciones: BTreeMap<String, u64>,
    pub configuracion: BTreeMap<String, u64>,
    pub bi: BTreeMap<String, u64>,
}

impl V4LobeTypeCounts {
    pub fn from_counts(c: &ExpectedCounts) -> Self {
        let mut clientes = BTreeMap::new();
        clientes.insert("_total".to_string(), c.clients);

        let mut creditos = BTreeMap::new();
        creditos.insert("Credit".to_string(), c.credits);
        creditos.insert("Installment".to_string(), c.installments);
        creditos.insert("Payment".to_string(), c.payments);
        creditos.insert("Collection".to_string(), c.collections);
        creditos.insert("CollectionAction".to_string(), c.collection_actions);

        let mut operaciones = BTreeMap::new();
        operaciones.insert("CreditApplication".to_string(), c.applications);
        operaciones.insert("AuditLog".to_string(), c.audit_log);
        operaciones.insert("Notification".to_string(), c.notifications);

        let mut configuracion = BTreeMap::new();
        configuracion.insert("Empresa".to_string(), c.empresas);
        configuracion.insert("Producto".to_string(), c.productos);

        let mut bi = BTreeMap::new();
        bi.insert("_total".to_string(), c.bi_snapshots);

        Self {
            clientes,
            creditos,
            operaciones,
            configuracion,
            bi,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct V6ConfigCounts {
    pub empresas: u64,
    pub productos: u64,
    /// Sum of empresas + productos. Redundant with the two scalars above
    /// but explicit per the cross-engine bench design notes §12.3 V6 specification.
    #[serde(rename = "_total")]
    pub total: u64,
}

// ── Session 2 — verify_golden return shape ───────────────────────────────

/// Per-driver `verify_golden` output. Carried into the run report's
/// `golden_match` / `golden_diffs` fields by the orchestrator.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GoldenVerifyResults {
    /// True iff `diffs` is empty (no V-query exceeded tolerance).
    pub overall_match: bool,
    /// One entry per (V-query, field) pair that failed comparison.
    /// Empty when `overall_match == true`.
    pub diffs: Vec<GoldenDiff>,
}

/// One mismatched V-query field. Counts cast to f64 so the same struct
/// covers both `n` (integer cardinality) and `sum` (monetary aggregate).
/// `relative_delta` is `(observed - expected).abs() / expected.max(1.0)`
/// for sums; for counts it is computed identically (will be 0.0 on exact
/// match, ≥1.0 on missing rows).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenDiff {
    /// V-query identifier — one of "V1_credits_total",
    /// "V2_installments_overdue", "V3_payments_total",
    /// "V4_lobe_type:<lobe>:<type>", "V5_clients_distinct_rfc",
    /// "V6_config:<empresas|productos|_total>".
    pub query: String,
    /// Field within the V-query — "n" or "sum".
    pub field: String,
    pub expected: f64,
    pub observed: f64,
    pub relative_delta: f64,
}

/// Helper: compare a single (n, sum) pair against a golden
/// `AggregateCountSum` and append diffs to `out` for any mismatch.
/// Counts must match exactly; sums use `tolerance` as a relative bound.
pub fn compare_count_sum(
    query: &str,
    expected: &AggregateCountSum,
    observed_n: u64,
    observed_sum: f64,
    tolerance: f64,
    out: &mut Vec<GoldenDiff>,
) {
    if observed_n != expected.n {
        let exp = expected.n as f64;
        out.push(GoldenDiff {
            query: query.to_string(),
            field: "n".to_string(),
            expected: exp,
            observed: observed_n as f64,
            relative_delta: (observed_n as f64 - exp).abs() / exp.max(1.0),
        });
    }
    let delta = (observed_sum - expected.sum).abs() / expected.sum.abs().max(1.0);
    if delta > tolerance {
        out.push(GoldenDiff {
            query: query.to_string(),
            field: "sum".to_string(),
            expected: expected.sum,
            observed: observed_sum,
            relative_delta: delta,
        });
    }
}

/// Helper: compare a single count against a golden `AggregateCount`.
pub fn compare_count(
    query: &str,
    expected: &AggregateCount,
    observed_n: u64,
    out: &mut Vec<GoldenDiff>,
) {
    if observed_n != expected.n {
        let exp = expected.n as f64;
        out.push(GoldenDiff {
            query: query.to_string(),
            field: "n".to_string(),
            expected: exp,
            observed: observed_n as f64,
            relative_delta: (observed_n as f64 - exp).abs() / exp.max(1.0),
        });
    }
}
