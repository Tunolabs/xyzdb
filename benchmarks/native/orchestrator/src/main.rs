//! Native-bench orchestrator.
//!
//! CLI:
//!   native-bench --engine xyzdb|postgres
//!                --scale 0.1
//!                --storage ssd|hdd
//!                --schema-mode full|auto-only
//!                [--phase all|setup|load|cold|concurrent|verify]
//!                [--duration <secs>]
//!                [--output ./results]

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use native_driver_mongo::MongoDriver;
use native_driver_postgres::PostgresDriver;
use native_driver_xyzdb::XyzdbDriver;
use native_generator::Dataset;
use native_generator::bench::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod metrics;
mod report;
mod resources;

#[derive(Parser, Debug)]
#[command(version, about = "native cross-engine benchmark")]
struct Args {
    /// Engine to bench.
    #[arg(long, value_enum)]
    engine: EngineArg,

    /// Scale: 0.1 = ~14.7 M records (primary), 1.0 = ~149 M records.
    #[arg(long, default_value_t = 0.1)]
    scale: f64,

    /// Storage profile (passed to the engine container; bench reports it).
    #[arg(long, value_enum, default_value_t = StorageArg::Ssd)]
    storage: StorageArg,

    /// Schema mode: full (default — pre-create ghosts/mat-views) or
    /// auto-only (Phase 6 variant — skip pre-aggregation, validate
    /// auto-promotion).
    #[arg(long, value_enum, default_value_t = SchemaModeArg::Full)]
    schema_mode: SchemaModeArg,

    /// Phase selector. `all` runs setup + load + cold + concurrent + verify.
    #[arg(long, default_value = "all")]
    phase: String,

    /// Concurrent phase duration (seconds). 0 → skip concurrent phase.
    #[arg(long, default_value_t = 3600u64)]
    duration: u64,

    /// Concurrent phase reader count. Default 8 (canonical Bench A
    /// protocol). Exposing the existing `ConcurrentProfile.readers`
    /// field as a CLI flag enables parametric studies of the contention
    /// surface without re-building the binary. Originally added for
    /// v0.3.2 Spike B (single-reader vs 8-reader decomposition).
    #[arg(long, default_value_t = 8usize)]
    readers: usize,

    /// Random seed (defaults to 42).
    #[arg(long, default_value_t = 42u64)]
    seed: u64,

    /// xyzDB host:port (default 127.0.0.1:2505).
    #[arg(long, default_value = "127.0.0.1:2505")]
    xyzdb_addr: String,

    /// PostgreSQL connection string.
    #[arg(
        long,
        default_value = "host=127.0.0.1 port=5432 user=postgres password=bench dbname=bench"
    )]
    pg_conn: String,

    /// MongoDB connection URI.
    #[arg(long, default_value = "mongodb://127.0.0.1:27017")]
    mongo_uri: String,

    /// MongoDB database name.
    #[arg(long, default_value = "bench")]
    mongo_db: String,

    /// Output directory for JSON / CSV / MD reports.
    #[arg(long, default_value = "./results")]
    output: PathBuf,

    /// Path to the verify-golden file produced by `golden_dump`. If
    /// empty, the orchestrator looks for
    /// `<output>/golden-scale<X>-seed<Y>.json` (Phase E Session 1
    /// convention). When the file is absent, `verify_golden` is skipped
    /// and a WARN is logged — downstream gates treat the absence as
    /// integrity-pending, NOT as PASS, per
    /// the cross-engine bench design notes §12.3 Verify-golden methodology.
    #[arg(long, default_value = "")]
    golden: String,

    /// Number of cold-query repeats per Q.
    #[arg(long, default_value_t = 100usize)]
    cold_runs: usize,

    /// Container name to sample with `docker stats`. If empty, defaults
    /// to `native-{xyzdb|postgres|mongodb}-1` per engine.
    #[arg(long, default_value = "")]
    container_name: String,

    /// Host path of the engine's data dir for `du -sk` sampling. If
    /// empty, defaults to `./data/{xyzdata|pgdata|mongodata}`.
    #[arg(long, default_value = "")]
    data_path: String,

    /// Disable resource sampling (CPU% / mem / disk).
    #[arg(long, default_value_t = false)]
    no_resources: bool,

    /// Engine image / CPU architecture label recorded in the report, e.g.
    /// `x86-v3` or `arm`. If empty, falls back to the `XYZ_IMAGE_VARIANT`
    /// env var the image-matrix runner sets when it brings the container up,
    /// so the report's label matches the image that actually ran.
    #[arg(long, default_value = "")]
    engine_image: String,

    /// v0.5 multi-persona — per-thread persona assignment. Format:
    /// `front=N,batch=M,analytics=K,regulatorio=L`. Default `4-2-2-1`
    /// for 9 threads (canonical fintech ERP heterogeneous workload).
    /// Sum must be ≤ total threads. Missing keys default to 0; the
    /// remainder is left idle. Pass `none` to disable personas
    /// entirely (legacy uniform workload).
    #[arg(long, default_value = "front=4,batch=2,analytics=2,regulatorio=1")]
    personas: String,

    /// v0.5 schedule — time-of-day intensity modulation. Path to a
    /// YAML file. Special value `daily_erp` selects the built-in
    /// fintech ERP daily pattern; `none` disables modulation (uniform
    /// 1.0× across the run, matches pre-v0.5 behaviour).
    #[arg(long, default_value = "daily_erp")]
    schedule: String,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum EngineArg {
    Xyzdb,
    Postgres,
    Mongo,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum StorageArg {
    Ssd,
    Hdd,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SchemaModeArg {
    Full,
    AutoOnly,
}

impl From<SchemaModeArg> for SchemaMode {
    fn from(v: SchemaModeArg) -> Self {
        match v {
            SchemaModeArg::Full => SchemaMode::Full,
            SchemaModeArg::AutoOnly => SchemaMode::AutoOnly,
        }
    }
}

impl From<StorageArg> for StorageProfile {
    fn from(v: StorageArg) -> Self {
        match v {
            StorageArg::Ssd => StorageProfile::Ssd,
            StorageArg::Hdd => StorageProfile::Hdd,
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("create output dir {:?}", args.output))?;

    let dataset = Dataset::new(args.seed, args.scale);
    let expected = dataset.expected_counts();
    info!(target: "orch",
          "scale={} engine={:?} storage={:?} schema_mode={:?} expected total records={}",
          args.scale, args.engine, args.storage, args.schema_mode, expected.total());

    let started_at = Utc::now();

    let driver: Box<dyn NativeDriver> = match args.engine {
        EngineArg::Xyzdb => {
            let (host, port) = parse_addr(&args.xyzdb_addr)?;
            Box::new(XyzdbDriver::new(host, port))
        }
        EngineArg::Postgres => {
            Box::new(PostgresDriver::new(&args.pg_conn).context("PG driver init")?)
        }
        EngineArg::Mongo => Box::new(
            MongoDriver::new(&args.mongo_uri, &args.mongo_db).context("Mongo driver init")?,
        ),
    };

    let phases = parse_phases(&args.phase);

    let mut schema_metrics: Option<SchemaMetrics> = None;
    let mut load_metrics: Option<LoadMetrics> = None;
    let mut cold_query_stats: Vec<QueryStats> = Vec::new();
    let mut concurrent_results: Option<ConcurrentResults> = None;
    let mut verify_results: Option<VerifyResults> = None;
    let mut verify_golden_results: Option<native_generator::GoldenVerifyResults> = None;
    let mut content_gate_results: Option<native_generator::bench::ContentGateResults> = None;

    let (sampler_container, sampler_path) = resolve_sampler_targets(&args);
    let sampler = if args.no_resources {
        None
    } else {
        info!(target: "orch", "resource sampler: container={} data_path={}",
              if sampler_container.is_empty() { "(disabled)" } else { sampler_container.as_str() },
              if sampler_path.is_empty() { "(none)" } else { sampler_path.as_str() });
        Some(resources::ResourceSampler::start(
            &sampler_container,
            &sampler_path,
            Instant::now(),
        ))
    };

    if phases.contains(&Phase::Setup) {
        info!(target: "orch", "── Phase 0: schema setup ──");
        if let Some(s) = &sampler {
            s.set_phase("phase0");
        }
        schema_metrics = Some(driver.setup_schema(args.schema_mode.into())?);
    }
    if phases.contains(&Phase::Load) {
        info!(target: "orch", "── Phase 1: bulk load ──");
        if let Some(s) = &sampler {
            s.set_phase("phase1");
        }
        load_metrics = Some(driver.bulk_load(&dataset)?);
        info!(target: "orch", "── Phase 0.5: post-load ──");
        if let Some(s) = &sampler {
            s.set_phase("post_load");
        }
        driver.post_load()?;

        // Phase 1.5 — verify_golden (Phase E Session 2 + caveat C-9).
        // Runs immediately after post_load() so the engine state is the
        // pristine bulk_load result with zero mutation; this gate
        // answers cleanly "did the engine ingest correctly?" without
        // contamination from cold-query writes (Q7 PUT BATCH × 100 rows ×
        // cold_runs = ~Δ+10,000 records under the default --cold-runs=100). See
        // the cross-engine bench design notes §12.3 caveat C-9 for the incident that
        // moved this gate from Phase 5b to Phase 1.5 + the rationale.
        let golden_path = resolve_golden_path(&args);
        if golden_path.exists() {
            info!(target: "orch", "── Phase 1.5: verify_golden vs {:?} ──", golden_path);
            if let Some(s) = &sampler {
                s.set_phase("phase1_5");
            }
            match std::fs::read_to_string(&golden_path)
                .with_context(|| format!("read {:?}", golden_path))
                .and_then(|t| {
                    serde_json::from_str::<native_generator::GoldenFile>(&t)
                        .with_context(|| format!("parse golden {:?}", golden_path))
                }) {
                Ok(golden) => match driver.verify_golden(&golden) {
                    Ok(res) => {
                        info!(target: "orch",
                              "verify_golden: match={} diffs={}",
                              res.overall_match, res.diffs.len());
                        for d in &res.diffs {
                            warn!(target: "orch",
                                  "  diff {} {}: expected={:.2} observed={:.2} rel_delta={:.6}",
                                  d.query, d.field, d.expected, d.observed, d.relative_delta);
                        }
                        verify_golden_results = Some(res);
                    }
                    Err(e) => {
                        warn!(target: "orch", "verify_golden invocation failed: {e:#}");
                    }
                },
                Err(e) => {
                    warn!(target: "orch", "could not load golden file: {e:#}");
                }
            }
        } else {
            warn!(target: "orch",
                  "no golden file at {:?} — Phase 1.5 verify_golden skipped (run `golden_dump` first to enable)",
                  golden_path);
        }
    }
    if phases.contains(&Phase::Cold) {
        info!(target: "orch", "── Phase 2: cold queries ({} runs each) ──", args.cold_runs);
        if let Some(s) = &sampler {
            s.set_phase("phase2");
        }
        cold_query_stats = run_cold_queries(driver.as_ref(), &dataset, args.cold_runs)?;
    }
    if phases.contains(&Phase::Concurrent) && args.duration > 0 {
        info!(target: "orch", "── Phase 3: concurrent workload ({} s) ──", args.duration);
        if let Some(s) = &sampler {
            s.set_phase("phase3");
        }
        // Sample RFC pool from clients stream (deterministic).
        let pool: Vec<String> = dataset.clients().take(10_000).map(|c| c.rfc).collect();
        let mut profile = ConcurrentProfile::fintech_default();
        profile.duration = Duration::from_secs(args.duration);
        profile.readers = args.readers;

        // v0.5 — parse persona assignment from `--personas` flag.
        let total_threads = profile.readers + profile.writers;
        profile.persona_assignment = match args.personas.trim() {
            "" | "none" => None,
            spec => {
                let pa = native_generator::personas::PersonaAssignment::parse(spec, total_threads)
                    .map_err(|e| anyhow::anyhow!("--personas: {e}"))?;
                info!(target: "orch", "v0.5 personas: {}", spec);
                Some(pa)
            }
        };

        // v0.5 — load schedule from `--schedule` flag.
        profile.schedule = match args.schedule.trim() {
            "" | "none" => None,
            "daily_erp" => {
                info!(target: "orch", "v0.5 schedule: daily_erp (built-in)");
                Some(native_generator::schedule::Schedule::daily_erp())
            }
            path => {
                let s = native_generator::schedule::Schedule::from_yaml_file(std::path::Path::new(
                    path,
                ))
                .map_err(|e| anyhow::anyhow!("--schedule: {e}"))?;
                info!(target: "orch", "v0.5 schedule: loaded from {}", path);
                Some(s)
            }
        };
        if matches!(args.engine, EngineArg::Postgres | EngineArg::Mongo) {
            // v0.3.3 Phase 2 verification refinement (Scenario C):
            // Per design SPEC §8.B5 / §8.C5 literal: 3 background threads
            // with cadences {30, 30, 60}. Drivers use enumerate-by-index
            // dispatch (lib.rs lines below) to map each thread to a
            // distinct mat-view / pre-agg target. Vector ORDER matters:
            //   index 0 (30s) → overdue_by_empresa_mat / overdue_by_empresa_agg
            //   index 1 (30s) → credits_by_rfc_mat / credits_by_rfc
            //   index 2 (60s) → monthly_close_mat / monthly_close_agg
            // Pre-refinement orchestrator config was vec![30, 60] which
            // (combined with bucket-keyed dispatch) caused credits_by_rfc
            // to never refresh in Phase 3 (legacy v0.2.5 latent bug; see
            // Phase 2 verification report).
            profile.refresh_cadences_secs = vec![30, 30, 60];
        }
        concurrent_results = Some(driver.run_concurrent(&profile, &pool)?);
    }
    if phases.contains(&Phase::Verify) {
        info!(target: "orch", "── Phase 5: integrity verify (legacy lobe-level gate) ──");
        if let Some(s) = &sampler {
            s.set_phase("phase5");
        }
        verify_results = Some(driver.verify(&expected)?);

        // Phase 5b — append-invariant content gate (v0.7.2). Runs after the
        // concurrent workload so it is the stable correctness signal the
        // cardinality verify above cannot be (that one drifts with Phase 3
        // appends). Skipped engines return `ran = false` and never fail.
        info!(target: "orch", "── Phase 5b: content gate (append-invariant) ──");
        match driver.verify_content_gate(&dataset) {
            Ok(res) => {
                if res.ran {
                    info!(target: "orch",
                          "content gate: overall_match={} scope={}",
                          res.overall_match, res.scope);
                    for l in &res.lobes {
                        if l.matched {
                            info!(target: "orch",
                                  "  {} OK ({} records, hash={})",
                                  l.lobe, l.records_hashed, l.observed_hash);
                        } else {
                            warn!(target: "orch",
                                  "  {} MISMATCH ({} records, expected={} observed={})",
                                  l.lobe, l.records_hashed, l.expected_hash, l.observed_hash);
                        }
                    }
                } else {
                    info!(target: "orch", "content gate skipped (engine opt-out)");
                }
                content_gate_results = Some(res);
            }
            Err(e) => {
                warn!(target: "orch", "content gate invocation failed: {e:#}");
            }
        }
    }

    let resource_metrics = sampler.map(|s| s.stop());

    let finished_at = Utc::now();

    let report = RunReport {
        engine: match args.engine {
            EngineArg::Xyzdb => EngineKind::Xyzdb,
            EngineArg::Postgres => EngineKind::Postgres,
            EngineArg::Mongo => EngineKind::Mongo,
        },
        storage: args.storage.into(),
        scale: args.scale,
        schema_mode: args.schema_mode.into(),
        schema: schema_metrics.unwrap_or(SchemaMetrics {
            mode: args.schema_mode.into(),
            setup_statements: 0,
            setup_duration_ms: 0,
        }),
        load: load_metrics.unwrap_or(LoadMetrics {
            records_loaded: 0,
            duration_ms: 0,
            records_per_sec: 0.0,
        }),
        cold_queries: cold_query_stats,
        concurrent: concurrent_results,
        verify: verify_results.unwrap_or(VerifyResults {
            exact: false,
            diffs: vec![],
        }),
        verify_golden: verify_golden_results,
        content_gate: content_gate_results,
        resources: resource_metrics,
        engine_image: if args.engine_image.is_empty() {
            std::env::var("XYZ_IMAGE_VARIANT").unwrap_or_default()
        } else {
            args.engine_image.clone()
        },
        started_at,
        finished_at,
    };

    let run_id = format!(
        "{}-{}-scale{}-{}",
        report.engine.as_str(),
        report.storage.as_str(),
        format_scale(args.scale),
        started_at.format("%Y%m%d-%H%M%S"),
    );

    report::write_json(&args.output, &run_id, &report)?;
    report::write_csv(&args.output, &run_id, &report)?;
    report::write_markdown(&args.output, &run_id, &report)?;

    info!(target: "orch", "Run complete: {}", run_id);
    info!(target: "orch", "Reports: {}/{}.{{json,csv,md}}", args.output.display(), run_id);

    Ok(())
}

fn run_cold_queries(
    driver: &dyn NativeDriver,
    dataset: &Dataset,
    n_runs: usize,
) -> Result<Vec<QueryStats>> {
    // Pool of RFCs to sample for params.
    let rfcs: Vec<String> = dataset.clients().take(2_000).map(|c| c.rfc).collect();
    if rfcs.is_empty() {
        bail!("no clients in dataset; cannot sample RFCs for cold queries");
    }
    let mut out = Vec::new();
    for &q in BusinessQuery::cold_queries() {
        let mut samples = Vec::with_capacity(n_runs);
        let mut records = Vec::with_capacity(n_runs);
        for i in 0..n_runs {
            let rfc = &rfcs[i % rfcs.len()];
            let params = QueryParams {
                rfc: rfc.clone(),
                monto_threshold: 50_000.0,
                limit: 100,
            };
            match driver.run_query(q, &params) {
                Ok(exec) => {
                    samples.push(exec.latency_ms);
                    records.push(exec.records_returned);
                }
                Err(e) => {
                    warn!(target: "orch", "{} run {} error: {}", q.as_str(), i, e);
                }
            }
        }
        let stats = QueryStats::from_samples(q, &samples, &records);
        info!(target: "orch",
              "{}: P50={:.2}ms  P95={:.2}ms  P99={:.2}ms  avg={:.2}ms  n={}",
              stats.query, stats.p50_ms, stats.p95_ms, stats.p99_ms, stats.avg_ms, stats.n_runs);
        out.push(stats);
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Setup,
    Load,
    Cold,
    Concurrent,
    Verify,
}

fn parse_phases(s: &str) -> Vec<Phase> {
    if s == "all" {
        return vec![
            Phase::Setup,
            Phase::Load,
            Phase::Cold,
            Phase::Concurrent,
            Phase::Verify,
        ];
    }
    s.split(',')
        .filter_map(|p| match p.trim() {
            "setup" => Some(Phase::Setup),
            "load" => Some(Phase::Load),
            "cold" => Some(Phase::Cold),
            "concurrent" => Some(Phase::Concurrent),
            "verify" => Some(Phase::Verify),
            other => {
                warn!(target: "orch", "unknown phase '{}'", other);
                None
            }
        })
        .collect()
}

fn parse_addr(s: &str) -> Result<(String, u16)> {
    let mut parts = s.splitn(2, ':');
    let host = parts.next().context("addr missing host")?.to_string();
    let port: u16 = parts
        .next()
        .context("addr missing port")?
        .parse()
        .context("addr port not u16")?;
    Ok((host, port))
}

/// Returns `(container_name, data_path)` for the `ResourceSampler`.
/// Defaults pick `native-{xyzdb|postgres|mongodb}-1` and the standard
/// bind-mount source under `./data/`. If the user passed explicit
/// `--container-name` / `--data-path`, those override. The `XYZ_DATA`
/// / `PG_DATA` / `MONGO_DATA` environment variables (used by
/// `docker compose` to point bind mounts at e.g. an external HDD)
/// are honoured for the data-path default so HDD runs sample the
/// physical disk rather than a stale local placeholder.
fn resolve_sampler_targets(args: &Args) -> (String, String) {
    let container = if !args.container_name.is_empty() {
        args.container_name.clone()
    } else {
        match args.engine {
            EngineArg::Xyzdb => "native-xyzdb-1".to_string(),
            EngineArg::Postgres => "native-postgres-1".to_string(),
            EngineArg::Mongo => "native-mongodb-1".to_string(),
        }
    };
    let data_path = if !args.data_path.is_empty() {
        args.data_path.clone()
    } else {
        match args.engine {
            EngineArg::Xyzdb => {
                std::env::var("XYZ_DATA").unwrap_or_else(|_| "./data/xyzdata".to_string())
            }
            EngineArg::Postgres => {
                std::env::var("PG_DATA").unwrap_or_else(|_| "./data/pgdata".to_string())
            }
            EngineArg::Mongo => {
                std::env::var("MONGO_DATA").unwrap_or_else(|_| "./data/mongodata".to_string())
            }
        }
    };
    (container, data_path)
}

/// Compute the golden-file path: explicit `--golden` arg takes
/// precedence; otherwise default to `<output>/golden-scale<X>-seed<Y>
/// .json` matching the convention emitted by `golden_dump`. The scale
/// formatting mirrors `golden_dump.rs::main` so the file name is
/// stable across both producer and consumer.
fn resolve_golden_path(args: &Args) -> PathBuf {
    if !args.golden.is_empty() {
        return PathBuf::from(&args.golden);
    }
    args.output
        .join(format!("golden-scale{}-seed{}.json", args.scale, args.seed))
}

fn format_scale(s: f64) -> String {
    if (s - 1.0).abs() < 1e-6 {
        "1.0".to_string()
    } else if (s - 0.1).abs() < 1e-6 {
        "0.1".to_string()
    } else {
        format!("{:.4}", s)
    }
}

// Suppress the "instant unused" lint when the function is small.
#[allow(dead_code)]
fn unused_instant() -> Instant {
    Instant::now()
}

#[allow(dead_code)]
fn unused_arc<T>(t: T) -> Arc<T> {
    Arc::new(t)
}
