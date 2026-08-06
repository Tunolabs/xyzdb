//! Shared bench types — `BusinessQuery` enum, query params, metrics
//! structs, run profiles. Lives in the generator crate so both drivers
//! and the orchestrator share one source of truth.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The nine business questions of the native bench (Q1-Q6 carried
/// from v0.2.5 with audit fairness fixes; Q7-Q9 NEW per the v0.3.3 bench design). Q10 (transactional cascade)
/// was removed: it was deferred on xyzDB (parser arithmetic-on-RHS) and
/// Mongo (replicaSet), so only PG ever measured it — an asymmetric cell
/// that never belonged in a cross-engine comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BusinessQuery {
    /// Q1 — Point lookup by RFC.
    Q1Point,
    /// Q2 — Total credit exposure for a given client.
    Q2Aggregate,
    /// Q3 — Complete portfolio history for a given client.
    Q3FullHistory,
    /// Q4 — Top 100 clients by total active exposure.
    Q4TopExposure,
    /// Q5 — Overdue installments by branch (`empresa_id`).
    Q5OverdueByEmpresa,
    /// Q6 — Recent payments above threshold, paginated.
    Q6RecentPayments,
    /// Q7 — Batch ingest 100 payments (NEW v0.3.3 §7.8). Replaces the
    /// v0.2.5 `Q7Concurrent` placeholder which was never a discrete query;
    /// the Phase 3 sustained concurrent workload is dispatched by the
    /// orchestrator separately, not as a `BusinessQuery` variant.
    Q7BatchIngest,
    /// Q8 — Monthly close composite per empresa (NEW v0.3.3 §7.9).
    Q8MonthlyClose,
    /// Q9 — Customer 360 context pull (NEW v0.3.3 §7.10).
    Q9CustomerContext,
}

impl BusinessQuery {
    pub fn as_str(&self) -> &'static str {
        match self {
            BusinessQuery::Q1Point => "Q1Point",
            BusinessQuery::Q2Aggregate => "Q2Aggregate",
            BusinessQuery::Q3FullHistory => "Q3FullHistory",
            BusinessQuery::Q4TopExposure => "Q4TopExposure",
            BusinessQuery::Q5OverdueByEmpresa => "Q5OverdueByEmpresa",
            BusinessQuery::Q6RecentPayments => "Q6RecentPayments",
            BusinessQuery::Q7BatchIngest => "Q7BatchIngest",
            BusinessQuery::Q8MonthlyClose => "Q8MonthlyClose",
            BusinessQuery::Q9CustomerContext => "Q9CustomerContext",
        }
    }

    /// Phase 2 cold-query entries. v0.3.3 expands from 6 (v0.2.5) to 9
    /// with Q7 batch ingest + Q8 monthly close (Phase 2.c) + Q9 customer
    /// 360 (Phase 2.d).
    pub fn cold_queries() -> &'static [BusinessQuery] {
        &[
            BusinessQuery::Q1Point,
            BusinessQuery::Q2Aggregate,
            BusinessQuery::Q3FullHistory,
            BusinessQuery::Q4TopExposure,
            BusinessQuery::Q5OverdueByEmpresa,
            BusinessQuery::Q6RecentPayments,
            BusinessQuery::Q7BatchIngest,
            BusinessQuery::Q8MonthlyClose,
            BusinessQuery::Q9CustomerContext,
        ]
    }
}

/// Per-query parameters sampled from the dataset for execution.
#[derive(Clone, Debug)]
pub struct QueryParams {
    /// Sampled RFC (used by Q1, Q2, Q3 and inside Q7's reader mix).
    pub rfc: String,
    /// Threshold for Q6 (`monto > threshold`). Default 50 000.
    pub monto_threshold: f64,
    /// Pagination limit for Q4 / Q6.
    pub limit: usize,
}

impl Default for QueryParams {
    fn default() -> Self {
        Self {
            rfc: String::new(),
            monto_threshold: 50_000.0,
            limit: 100,
        }
    }
}

/// Schema-mode flag: full = pre-create all ghosts / mat views (default);
/// auto-only = skip ghost / mat-view creation so telemetry promotes
/// organically (Phase 6 variant; xyzDB only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaMode {
    Full,
    AutoOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaMetrics {
    pub mode: SchemaMode,
    /// Authored setup declarations the driver issues to serve the benchmark
    /// queries — schema objects (tables/lobes), indexes, and pre-aggregations
    /// (xyzDB ghosts / PG mat-views / Mongo `$merge` pipelines). Counted
    /// UNIFORMLY across engines as **one per declaration** (NOT lines of DDL,
    /// NOT indexes-only — the pre-fix `ddl_lines` mixed all three, which made
    /// the cross-engine comparison meaningless). Excludes load-mode toggles
    /// (`BULKMODE`) and ongoing maintenance (`REFRESH` / `$merge` re-runs);
    /// excludes namespaces that auto-create on first write (Mongo collections).
    #[serde(alias = "ddl_lines")]
    pub setup_statements: usize,
    pub setup_duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadMetrics {
    pub records_loaded: u64,
    pub duration_ms: u64,
    pub records_per_sec: f64,
}

/// Per-query latency snapshot. All fields in milliseconds.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryStats {
    pub query: String,
    pub n_runs: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub avg_ms: f64,
    /// Records returned (sanity check vs expected).
    pub avg_records: f64,
    pub errors: usize,
    /// `true` when `n_runs > 0` AND `avg_records == 0` — the query
    /// executed without raising errors but produced an empty result
    /// set across every cold repetition. Surfaces the gate gap that
    /// silenced Surreal Q5 (Phase 5.b post-#14 smoke): refinement #15
    /// only failed cells with `n_runs == 0`; an empty-set Q with
    /// `n_runs > 0` slipped through. Refinement #16 (v0.3.4 cleanup
    /// cycle) marks these explicitly so downstream gates / readers
    /// flag them without having to re-derive from `avg_records`.
    pub empty_result_set: bool,
}

impl QueryStats {
    pub fn from_samples(query: BusinessQuery, samples_ms: &[f64], records: &[u64]) -> Self {
        if samples_ms.is_empty() {
            return Self {
                query: query.as_str().to_string(),
                ..Default::default()
            };
        }
        let mut sorted = samples_ms.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| -> f64 {
            let idx = ((sorted.len() as f64) * q).ceil() as usize;
            let idx = idx.saturating_sub(1).min(sorted.len() - 1);
            sorted[idx]
        };
        let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let avg_records = if records.is_empty() {
            0.0
        } else {
            records.iter().map(|x| *x as f64).sum::<f64>() / records.len() as f64
        };
        // Refinement #16: flag cold-phase Qs that ran without errors
        // but never returned any records. The avg_records == 0 check
        // uses an exact compare because `from_samples` accumulates
        // integer record counts via f64 sum / len; a true zero across
        // every sample yields exactly 0.0.
        let empty_result_set = !sorted.is_empty() && avg_records == 0.0;
        Self {
            query: query.as_str().to_string(),
            n_runs: sorted.len(),
            p50_ms: p(0.50),
            p95_ms: p(0.95),
            p99_ms: p(0.99),
            max_ms: *sorted.last().unwrap(),
            avg_ms: avg,
            avg_records,
            errors: 0,
            empty_result_set,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConcurrentProfile {
    pub readers: usize,
    pub writers: usize,
    pub duration: Duration,
    pub rampup: Duration,
    /// **Deprecated v0.3.3** per design §6.3: Zipf-skew RFC sampling is
    /// replaced by two-tier hot/cold 95/5 + Markov walk drift via
    /// `erratica::ErraticaConfig.hot_ratio` + `hot_query_prob`. Field
    /// kept for backwards-compat with v0.2.5 reference baselines (§5.1)
    /// — deserialisation of legacy result manifests still works.
    pub zipf_skew: f64,
    /// Refresh cadences (PG only). Empty for engines with incremental
    /// auto-update. v0.3.3 Phase 2 verification refinement: orchestrator
    /// passes `vec![30, 30, 60]` for PG/Mongo per design §8.B5/§8.C5
    /// SPEC literal; lib.rs dispatches by thread INDEX (Architecture β).
    pub refresh_cadences_secs: Vec<u64>,
    /// v0.3.3 workload errática parameters per design §6 (Phase 3
    /// implementation). Consumed by drivers' Phase 3 mixed-mode threads
    /// via `erratica::ErraticaPicker`.
    pub erratica: crate::erratica::ErraticaConfig,
    /// v0.5 multi-persona — per-thread persona assignment. Drivers
    /// look up `persona_assignment.persona_for(tid)` when constructing
    /// each thread's `ErraticaPicker`. `None` means uniform legacy
    /// behaviour (all threads share the same config without persona
    /// overrides).
    #[serde(skip)]
    pub persona_assignment: Option<crate::personas::PersonaAssignment>,
    /// v0.5 schedule — time-of-day intensity modulation. `None` means
    /// uniform 1.0× multiplier across the whole Phase 3.
    #[serde(skip)]
    pub schedule: Option<crate::schedule::Schedule>,
}

impl ConcurrentProfile {
    pub fn fintech_default() -> Self {
        let erratica = crate::erratica::ErraticaConfig::from_env();
        Self {
            readers: erratica.reader_threads,
            writers: erratica.writer_threads,
            duration: Duration::from_secs(erratica.phase3_duration_secs),
            rampup: Duration::from_secs(60),
            zipf_skew: 1.5,
            refresh_cadences_secs: vec![],
            erratica,
            persona_assignment: None,
            schedule: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConcurrentResults {
    pub reads_total: u64,
    pub writes_total: u64,
    pub reads_per_sec: f64,
    pub writes_per_sec: f64,
    /// Per-query stats observed during the run.
    pub per_query: Vec<QueryStats>,
    /// Coefficient-of-variation across 30-s windows. > 0.30 → unstable.
    pub throughput_cv: f64,
    /// PG only: number of refreshes executed and total wall-clock spent.
    pub refresh_count: u64,
    pub refresh_total_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifyResults {
    pub exact: bool,
    pub diffs: Vec<EntityDiff>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityDiff {
    pub entity: String,
    pub expected: u64,
    pub observed: u64,
}

/// Phase 5b content gate outcome. Unlike `VerifyResults` (cardinality)
/// and the golden V1-V6 aggregates (count/sum), this gate compares a
/// per-record **content** hash of the loaded immutable anchored entities
/// against the seed-regenerated expectation. It is invariant to Phase 3
/// appends by construction: the expectation is re-derived from the seed,
/// so appended rows (which carry brand-new keys) fall outside the hashed
/// set and are never looked up. This is the append-stable replacement for
/// the post-concurrent cardinality "exact" verdict.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentGateResults {
    /// True when the gate ran and every covered lobe's content hash matched.
    /// Defaults to `true` when the gate was skipped (a non-implementing
    /// engine must not fail the run).
    pub overall_match: bool,
    /// False when the engine does not implement the gate (skipped).
    pub ran: bool,
    /// Per-lobe content-hash comparison.
    pub lobes: Vec<ContentGateLobe>,
    /// Honest coverage label: which loaded data this gate hashes and what
    /// it does NOT (the known gap), so a green gate is never read as
    /// "everything was verified".
    pub scope: String,
}

/// One lobe's content-hash comparison within the content gate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentGateLobe {
    pub lobe: String,
    pub matched: bool,
    pub records_hashed: u64,
    /// Hex of the seed-regenerated expected fold.
    pub expected_hash: String,
    /// Hex of the engine read-back fold.
    pub observed_hash: String,
}

impl ContentGateResults {
    /// The default outcome for engines that do not implement the gate.
    /// `overall_match = true` so the run is not failed by a skip; `ran =
    /// false` lets downstream gates distinguish skipped from passed.
    pub fn skipped() -> Self {
        Self {
            overall_match: true,
            ran: false,
            lobes: Vec::new(),
            scope: "skipped: engine does not implement the content gate".to_string(),
        }
    }
}

/// Engine identity. Drivers declare which they are; orchestrator dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EngineKind {
    Xyzdb,
    Postgres,
    Mongo,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Xyzdb => "xyzdb",
            EngineKind::Postgres => "postgres",
            EngineKind::Mongo => "mongo",
        }
    }
}

/// Storage profile (passed to the engine container).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageProfile {
    Ssd,
    Hdd,
}

impl StorageProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            StorageProfile::Ssd => "ssd",
            StorageProfile::Hdd => "hdd",
        }
    }
}

/// Result of a single query execution. Used by the orchestrator to
/// build per-query statistics across N runs.
#[derive(Clone, Debug)]
pub struct QueryExecution {
    pub latency_ms: f64,
    pub records_returned: u64,
}

/// The driver contract every engine implements. Lives in the generator
/// crate so the orchestrator and drivers share one definition.
pub trait NativeDriver: Send + Sync {
    /// Engine identity.
    fn kind(&self) -> EngineKind;

    /// Phase 0: schema setup (DDL, indexes, ghosts/mat-views per mode).
    fn setup_schema(&self, mode: SchemaMode) -> anyhow::Result<SchemaMetrics>;

    /// Phase 1: bulk load. Returns throughput + duration.
    fn bulk_load(&self, dataset: &crate::Dataset) -> anyhow::Result<LoadMetrics>;

    /// Phase 0.5: post-load operations (xyzDB `AUTOANCHOR APPLY`,
    /// PG `ANALYZE` + initial REFRESH, etc.).
    fn post_load(&self) -> anyhow::Result<()>;

    /// Phase 2 / 4: execute a single business query and return its
    /// observed latency + record count.
    fn run_query(
        &self,
        query: BusinessQuery,
        params: &QueryParams,
    ) -> anyhow::Result<QueryExecution>;

    /// Phase 3: sustained concurrent workload.
    fn run_concurrent(
        &self,
        profile: &ConcurrentProfile,
        rfc_pool: &[String],
    ) -> anyhow::Result<ConcurrentResults>;

    /// Phase 5: integrity verify against expected record counts.
    fn verify(&self, expected: &crate::ExpectedCounts) -> anyhow::Result<VerifyResults>;

    /// Phase 1.5 (v0.3.4 Phase E Session 2 + caveat C-9 reschedule):
    /// verify the engine's observed V1-V6 aggregates against the
    /// generator-as-truth golden file.
    /// Each driver issues V1-V6 in its idiomatic form (xyzDB SCAN +
    /// AGGREGATE, PG SELECT count/sum, Mongo aggregation pipeline,
    /// Surreal SELECT count + math::sum) and compares to `golden` using
    /// `golden.tolerance_f64_relative` for sums + exact equality for
    /// counts. Per the cross-engine bench design notes Verify-golden methodology
    /// section: ingestion bugs surface here as `golden_diffs`, not as
    /// silent truth (caveat C-2 Surreal V2 is the canonical example).
    fn verify_golden(
        &self,
        golden: &crate::GoldenFile,
    ) -> anyhow::Result<crate::GoldenVerifyResults>;

    /// Phase 5b — content gate (append-invariant). Re-derive the loaded
    /// immutable anchored entities from the seed and compare a per-record
    /// content hash against the engine's read-back. Runs *after* the
    /// concurrent workload so it is the stable correctness signal that the
    /// cardinality verify cannot be (the latter drifts with Phase 3
    /// appends). Default impl: skipped, so only engines that opt in pay the
    /// cost and non-implementing engines never fail the run.
    fn verify_content_gate(&self, _dataset: &crate::Dataset) -> anyhow::Result<ContentGateResults> {
        Ok(ContentGateResults::skipped())
    }
}

/// One sample of container resource usage at a point in time.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceSample {
    /// Seconds since the orchestrator started (Instant t0).
    pub ts_secs: f64,
    /// Phase tag at sample time (`phase0`, `phase1`, `post_load`, `phase2`,
    /// `phase3`, `phase5`).
    pub phase: String,
    /// `docker stats` CPU%. May exceed 100 (one core = 100%; two-core
    /// cgroup peaks at ~200%).
    pub cpu_percent: f64,
    /// Container resident memory in MiB.
    pub mem_mb: f64,
    /// Last-known data-dir size in MiB. Disk is sampled less often than
    /// CPU/RAM (du is O(file count)), so this is the most recent du
    /// reading at sample time.
    pub disk_mb: f64,
}

/// Aggregate of `ResourceSample` series. Peak / avg / final values
/// suitable for cross-engine reporting.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceMetrics {
    /// Container name (`docker stats <container>`). Empty when sampling
    /// was disabled.
    pub container: String,
    /// Bind-mount source path (host side). Used for `du -sb`.
    pub data_path: String,
    /// Raw samples — useful for cross-phase plots after the fact.
    pub samples: Vec<ResourceSample>,
    pub cpu_peak: f64,
    pub cpu_avg: f64,
    pub mem_peak_mb: f64,
    pub mem_avg_mb: f64,
    pub disk_peak_mb: f64,
    /// Disk size at sampler stop (post-Phase 5).
    pub disk_final_mb: f64,
    pub n_samples: usize,
}

/// The full protocol, against which a run is judged comparable.
///
/// 100 cold repeats and a 300-second concurrent phase: the values the canonical
/// launcher passes and the ones the published series were produced with. They are
/// named here, next to the flag, so the definition cannot drift from the check.
pub const CANONICAL_COLD_RUNS: usize = 100;
pub const CANONICAL_CONCURRENT_SECONDS: u64 = 300;

/// Aggregate run report. One per orchestrator invocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub engine: EngineKind,
    pub storage: StorageProfile,
    pub scale: f64,
    /// Whether this run used the FULL protocol, and is therefore usable as a
    /// baseline for comparison against another run.
    ///
    /// A reduced run (fewer cold repeats, a shorter concurrent phase) produces a
    /// report that is identical in every visible respect — same filename shape,
    /// same fields, same structure — and differs only in the sample counts buried
    /// inside it. Comparing a p50 over 20 samples against one over 100 is not a
    /// comparison, and the mistake is invisible at the point where it is made.
    ///
    /// So the run declares it rather than leaving a reader to reconstruct it:
    /// `false` means *do not use this as a baseline*. Criteria in
    /// [`RunReport::is_canonical`].
    #[serde(default)]
    pub canonical: bool,
    /// Cold repeats per query, recorded so a reader can see WHY `canonical` is
    /// what it is instead of trusting the flag.
    #[serde(default)]
    pub cold_runs: usize,
    /// Concurrent phase length in seconds; `0` when the phase was skipped.
    #[serde(default)]
    pub concurrent_seconds: u64,
    pub schema_mode: SchemaMode,
    pub schema: SchemaMetrics,
    pub load: LoadMetrics,
    pub cold_queries: Vec<QueryStats>,
    pub concurrent: Option<ConcurrentResults>,
    pub verify: VerifyResults,
    /// Phase 1.5 verify_golden outcome — present only when the orchestrator
    /// found a golden file matching the run's (seed, scale) and was able
    /// to invoke `driver.verify_golden`. Absent when no golden file was
    /// available (treated as integrity-pending in downstream gates, not
    /// as PASS). v0.3.4 Phase E Session 2 introduces this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_golden: Option<crate::GoldenVerifyResults>,
    /// Phase 5b content gate outcome — append-invariant per-record content
    /// hash of the loaded immutable anchored entities. Absent when the
    /// Verify phase did not run; `ran = false` inside it when the engine
    /// does not implement the gate. v0.7.2 introduces this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_gate: Option<ContentGateResults>,
    /// Resource consumption summary; absent when `--no-resources` was
    /// passed or sampling failed to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceMetrics>,
    /// Which engine image / CPU architecture produced this run, e.g.
    /// `x86-v3` (linux/amd64, `target-cpu=x86-64-v3` / AVX2) or `arm`
    /// (linux/arm64, no x86 flag). Sourced from `--engine-image` or the
    /// `XYZ_IMAGE_VARIANT` env the runner sets when it brings the image up,
    /// so the label cannot drift from the container that actually ran. Empty
    /// when unset (e.g. bare-host or non-xyzDB engines). "x86-v3" vs "arm" is
    /// a real result axis; bit-identical recall is verified equal across both
    /// (the v2==v3 gate, `.cargo/config.toml`).
    #[serde(default)]
    pub engine_image: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}
