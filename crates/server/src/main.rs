// SPDX-License-Identifier: BUSL-1.1
// unwrap()/expect() are enforced on production code only. Test code — inline
// #[cfg(test)] modules and the integration tests under tests/ — may unwrap
// freely, since a panic there is the failure signal, not a defect. Gating on
// not(test) keeps `cargo clippy --all-targets` on real production debt.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use xyzdb_engine::engine::Engine;
use xyzdb_engine::throttle::ThrottleConfig;

// jemalloc returns memory to the OS more aggressively than glibc malloc and
// fragments less under LSM-style alloc/free churn. glibc malloc holds arenas
// indefinitely, inflating RSS by 15–30% on long-running compaction workloads.
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[derive(Parser)]
// `version` is not decoration: OPERATIONS.md §8.4 makes `xyzdb-server --version`
// the first diagnostic step of a format-mismatch incident, and without this the
// binary answered that step with "unexpected argument".
#[command(name = "xyzdb-server", version, about = "xyzDB database server")]
struct Args {
    /// Path to the database directory. Defaults to `./data/xyzdb` when
    /// unset.
    #[arg(long)]
    path: Option<PathBuf>,

    /// TCP port to listen on
    #[arg(long, default_value_t = 2505)]
    port: u16,

    /// Address to bind. Defaults to `127.0.0.1` (loopback): the server is not
    /// reachable off-host unless you change this. Binding a non-loopback address
    /// (e.g. `0.0.0.0`) with no `--auth-token` refuses to start, unless the
    /// explicit `--insecure-allow-no-auth` override is passed.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Throttle profile: transactional, analytical, balanced, maintenance, bulk
    #[arg(long, default_value = "balanced")]
    throttle_profile: String,

    /// Engine memory budget in MB — the single memory knob. Auto-detected from
    /// the cgroup limit if unset. Governs BOTH the block cache (a quarter of the
    /// budget) AND ingest: writes stall for background flush when the summed
    /// memtable footprint reaches ~35% of the budget, so a tight container bounds
    /// its own build instead of OOM-ing (budgets >= ~755 MB keep today's sizes).
    /// Override for loose binaries / sidecar reservation / benchmarks. Also
    /// settable via `XYZDB_MEMORY_BUDGET_MB` (this flag takes precedence).
    #[arg(long)]
    memory_budget_mb: Option<u64>,

    /// Deprecated: the block cache is now derived from `--memory-budget-mb`.
    /// Kept as a transitional override — when set it still forces the cache
    /// size in MB and bypasses the budget-derived value.
    #[arg(long, hide = true)]
    cache_size: Option<u64>,

    /// Storage profile: ssd (default) or hdd (256KB ghost blocks, bloom 14 bits).
    #[arg(long, default_value = "ssd")]
    storage_profile: String,

    /// I/O scheduler: ssd (default, Passthrough) or hdd (lane-aware).
    /// Independent from `--storage-profile` — operators may run an SSD-tuned
    /// engine on a rotational disk and choose the scheduler explicitly.
    /// Cycle doc §6 D6 (no auto-detect; explicit opt-in).
    #[arg(long, default_value = "ssd")]
    io_scheduler: String,

    /// Advanced tuning override for the L0 compaction batch size (H2.3
    /// §9.3). Default behaviour (no flag) uses the storage-profile
    /// default from `LeveledConfig::for_storage_profile`. Passing the
    /// flag overrides at runtime — primarily for the H2.3 sweep
    /// protocol and operators with workloads that diverge from the
    /// bench-frozen profile values.
    #[arg(long)]
    l0_batch: Option<usize>,

    /// v0.5.2 B.5: optional override for the WAL location. Defaults to
    /// `<path>/journal.wal` (co-located with data dir). When set, the WAL
    /// lives at this exact path; the rest of the data dir (SSTs,
    /// MANIFEST, snapshots) remains under `--path`. The two paths should
    /// share a filesystem (snapshot hard-link orchestration assumes it).
    #[arg(long)]
    wal_path: Option<PathBuf>,

    /// Durability mode: durable (fsync every write), batched (fsync every N ms), async (OS decides).
    #[arg(long, default_value = "durable")]
    durability: String,

    /// Interval in ms for batched durability mode (default: 100).
    #[arg(long, default_value_t = 100)]
    batch_interval: u64,

    /// RecordCache budget in MB (default: 0 = disabled). Records loaded via INCACHE.
    /// `--hot-cache-size` is retained as a deprecated alias.
    #[arg(long, alias = "hot-cache-size", default_value_t = 0)]
    record_cache_size: u64,

    /// NEAREST time-budget airbag in ms (default 3000; 0 disables). It is a
    /// LATENCY wall, never a recall wall, and what happens on expiry depends on
    /// the path: a bounded NEAREST returns the best-scoring rows found so far
    /// with a `budget_stop` object describing the cut, while an unbounded
    /// scoring scan still aborts with a clear error instead of hanging.
    /// Calibrated to the worst dimensioned bucket (1536d/250k: p99 ~1505ms,
    /// max ~2502ms) with margin; guards runaway buckets, not normal queries.
    #[arg(long, default_value_t = 3000)]
    nearest_budget_ms: u64,

    /// Override auto-ghost trigger: minimum hits in the 10-min window.
    /// Default (unset) = 5. Useful for tuning sensitivity or setting a
    /// high value to effectively disable auto-ghost for baseline runs.
    #[arg(long)]
    auto_ghost_min_hits: Option<u64>,

    /// Override auto-ghost trigger: minimum average latency in ms for a
    /// pattern to qualify. Default (unset) = 20.0. Pass a large value
    /// (e.g. 1e9) to effectively disable auto-ghost creation — used by
    /// the Zipf benchmark matrix to measure "cost of having ghosts
    /// enabled" independent of the wins auto-ghost produces.
    #[arg(long)]
    auto_ghost_min_latency_ms: Option<f64>,

    /// v0.4 item 3: path to PEM-encoded TLS server certificate chain. If
    /// set together with `--tls-key`, the server accepts TLS 1.3
    /// connections; otherwise it serves plain TCP (with WARN at boot).
    /// Both flags must be provided together — passing only one is an
    /// error.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// v0.4 item 3: path to PEM-encoded private key (PKCS#8 or RSA). See
    /// `--tls-cert`.
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// Path to a UTF-8 file containing the bearer token the server requires
    /// from clients. Leading and trailing whitespace is trimmed. When set,
    /// every connection must present the matching token via the `AUTH_MAGIC`
    /// (`0x41`) preamble before the protocol version byte, and `STATS`,
    /// `SHOW STATS` and `/metrics` require it too; a mismatch closes the
    /// connection with an error frame. The `/health` and `/ready` liveness
    /// probes stay reachable without it.
    ///
    /// Without this flag the server accepts unauthenticated connections (and
    /// still consumes an auth frame a client sends, so `XYZDB_TOKEN` stays
    /// harmless). It is not exposed off-host regardless: the default bind is
    /// loopback, and a non-loopback bind with no token refuses to start
    /// unless `--insecure-allow-no-auth` is passed.
    ///
    /// The token is read from a plaintext file on disk.
    #[arg(long)]
    auth_token: Option<PathBuf>,

    /// Explicitly allow binding a non-loopback address with no `--auth-token`.
    /// This exposes an unauthenticated server to the network — only use it when
    /// access is controlled elsewhere (firewall, private network, service mesh).
    /// Without it, a non-loopback bind and no token is a hard startup error.
    #[arg(long)]
    insecure_allow_no_auth: bool,

    /// v0.4 cp 4.2.1 + 4.2.2: BlockCache lane-aware admission policy.
    /// When `enabled`, Compaction + Flush block-misses do NOT insert
    /// into the cache — they still benefit from cache hits previously
    /// warmed by user reads. When `disabled`, every miss admits
    /// regardless of lane (legacy v0.3.x behaviour).
    ///
    /// **Default: disabled** in v0.4. The cp 4.2.2 A/B microbench
    /// measured 0 % improvement in user-side hit rate under quick_cache
    /// 0.6's S3-FIFO eviction (which already protects hot user blocks
    /// from cold compaction churn). The policy is plumbed end-to-end
    /// and validated by unit tests; it is operationally redundant for
    /// the workloads exercised in v0.4 and stays off by default.
    /// Refinement (richer workload, alternative cache, A/B against a
    /// less-sophisticated eviction policy) is registered as cycle
    /// plan §8 finding H7 for v0.5 sub-cycle A or D.
    #[arg(long, value_enum, default_value_t = BlockCacheLaneAdmission::Disabled)]
    block_cache_lane_admission: BlockCacheLaneAdmission,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum BlockCacheLaneAdmission {
    Enabled,
    Disabled,
}

impl BlockCacheLaneAdmission {
    fn as_bool(self) -> bool {
        matches!(self, BlockCacheLaneAdmission::Enabled)
    }
}

/// Whether the server must refuse to start rather than expose an
/// unauthenticated listener to the network. True only for a **non-loopback**
/// bind with **no token** and **no explicit override** — the accidental-exposure
/// case. Loopback binds, an `--auth-token`, or `--insecure-allow-no-auth` all
/// return false. `localhost` and unspecified addresses (`0.0.0.0` / `::`) are
/// classified by their parsed [`std::net::IpAddr::is_loopback`]; an
/// unparseable bind string is treated as non-loopback (fail safe).
fn refuse_unauthenticated_bind(bind: &str, has_token: bool, insecure_override: bool) -> bool {
    let is_loopback = bind == "localhost"
        || bind
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    !is_loopback && !has_token && !insecure_override
}

/// Load PEM cert chain + private key and build a rustls `ServerConfig`.
/// TLS 1.3 only (no fallback). Errors are stringified for the operator-
/// facing log path; the caller decides whether to exit.
fn build_tls_config(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<tokio_rustls::rustls::ServerConfig, String> {
    use std::fs::File;
    use std::io::BufReader;

    let cert_file = File::open(cert_path)
        .map_err(|e| format!("open --tls-cert {}: {e}", cert_path.display()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("parse --tls-cert {}: {e}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "no certificates found in --tls-cert {}",
            cert_path.display()
        ));
    }

    let key_file =
        File::open(key_path).map_err(|e| format!("open --tls-key {}: {e}", key_path.display()))?;
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("parse --tls-key {}: {e}", key_path.display()))?
        .ok_or_else(|| format!("no private key found in --tls-key {}", key_path.display()))?;

    // TLS 1.3 only. rustls 0.23 default config supports both 1.2 and 1.3;
    // we restrict to 1.3 explicitly to match the documented surface.
    let config = tokio_rustls::rustls::ServerConfig::builder_with_protocol_versions(&[
        &tokio_rustls::rustls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| format!("rustls ServerConfig build: {e}"))?;

    Ok(config)
}

/// Resolve when the process is asked to stop: Ctrl-C (SIGINT) on any platform,
/// or SIGTERM on Unix (the signal an orchestrator / `systemctl stop` / `docker
/// stop` sends). Completing this future is the trigger for the graceful drain +
/// flush in the accept loop.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("SIGTERM handler install failed ({e}); Ctrl-C still works");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C"),
        _ = term => tracing::info!("received SIGTERM"),
    }
}

#[tokio::main]
async fn main() {
    // Honour RUST_LOG (EnvFilter); default to `info` when unset, preserving
    // prior behaviour. Benchmarks set RUST_LOG=warn to silence the per-query
    // plan_scan diagnostics that otherwise contaminate latency measurements.
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Resolve the data directory: the explicit `--path`, or the
    // historical default `./data/xyzdb` when unset.
    let resolved_path: PathBuf = args
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from("./data/xyzdb"));

    let throttle_config = ThrottleConfig::from_name(&args.throttle_profile).unwrap_or_else(|| {
        tracing::warn!(
            "Unknown throttle profile '{}', using 'balanced'",
            args.throttle_profile
        );
        ThrottleConfig::profile_balanced()
    });

    // Resolve the single memory knob: explicit flag > env > cgroup > default.
    // The flag takes precedence over the env var.
    let env_budget_mb =
        std::env::var("XYZDB_MEMORY_BUDGET_MB")
            .ok()
            .and_then(|s| match s.trim().parse::<u64>() {
                Ok(v) => Some(v),
                Err(_) => {
                    tracing::warn!(
                        "ignoring XYZDB_MEMORY_BUDGET_MB='{}': not a valid u64",
                        s.trim()
                    );
                    None
                }
            });
    let explicit_mb = args.memory_budget_mb.or(env_budget_mb);
    let budget = xyzdb_engine::memory_budget::resolve_memory_budget(explicit_mb);

    if budget.source == xyzdb_engine::memory_budget::BudgetSource::Default {
        tracing::warn!(
            "no memory limit detected (no --memory-budget-mb, no cgroup limit); using {} MB — pass --memory-budget-mb to tune",
            budget.bytes / (1024 * 1024)
        );
    } else {
        tracing::info!(
            "memory budget: {} MB (source: {:?})",
            budget.bytes / (1024 * 1024),
            budget.source
        );
    }

    // Cache is derived from the budget; `--cache-size` is a deprecated override.
    let cache_bytes = if let Some(mb) = args.cache_size {
        tracing::warn!(
            "--cache-size is deprecated; the block cache is derived from --memory-budget-mb. Using the {} MB override",
            mb
        );
        mb * 1024 * 1024
    } else {
        xyzdb_engine::memory_budget::cache_bytes_from_budget(budget.bytes)
    };
    let cache_desc = match args.cache_size {
        Some(mb) => format!("{mb} MB (--cache-size override)"),
        None => format!("{} MB (derived from budget)", cache_bytes / (1024 * 1024)),
    };

    let storage_profile = match args.storage_profile.to_lowercase().as_str() {
        "hdd" => Some(xyzdb_engine::keyspaces::StorageProfile::Hdd),
        "ssd" => Some(xyzdb_engine::keyspaces::StorageProfile::Ssd),
        _ => {
            tracing::warn!(
                "Unknown storage profile '{}', using 'ssd'",
                args.storage_profile
            );
            None
        }
    };

    let io_scheduler = match args.io_scheduler.to_lowercase().as_str() {
        "hdd" => Some(xyzdb_engine::keyspaces::IoSchedulerMode::Hdd),
        "ssd" => Some(xyzdb_engine::keyspaces::IoSchedulerMode::Ssd),
        _ => {
            tracing::warn!(
                "Unknown io-scheduler '{}', using 'ssd' (Passthrough)",
                args.io_scheduler
            );
            None
        }
    };

    let durability = match args.durability.to_lowercase().as_str() {
        "durable" => xyzdb_engine::engine::DurabilityMode::Durable,
        "batched" => xyzdb_engine::engine::DurabilityMode::Batched,
        "async" => xyzdb_engine::engine::DurabilityMode::Async,
        _ => {
            tracing::warn!("Unknown durability '{}', using 'durable'", args.durability);
            xyzdb_engine::engine::DurabilityMode::Durable
        }
    };

    tracing::info!(
        "Opening database at: {} (throttle: {}, cache: {}, storage: {}, durability: {:?})",
        resolved_path.display(),
        args.throttle_profile,
        cache_desc,
        args.storage_profile,
        durability,
    );

    if args.storage_profile.to_lowercase() == "hdd"
        && durability == xyzdb_engine::engine::DurabilityMode::Durable
    {
        tracing::info!("HDD + durable: consider --durability batched for higher write throughput");
    }

    let engine = match Engine::open_full(
        &resolved_path,
        throttle_config,
        Some(cache_bytes),
        storage_profile,
        Some(durability),
        io_scheduler,
        args.l0_batch,
        args.block_cache_lane_admission.as_bool(),
        args.wal_path.clone(),
        budget,
    ) {
        Ok(mut e) => {
            if args.record_cache_size > 0 {
                let budget = args.record_cache_size as usize * 1024 * 1024;
                e.set_record_cache_size(budget);
                tracing::info!("RecordCache enabled: {}MB budget", args.record_cache_size);
            }
            e.set_nearest_budget_ms(args.nearest_budget_ms);
            tracing::info!(
                "NEAREST budget: {}",
                if args.nearest_budget_ms == 0 {
                    "disabled".to_string()
                } else {
                    format!("{}ms", args.nearest_budget_ms)
                }
            );
            let arc = e.into_arc();
            if args.auto_ghost_min_hits.is_some() || args.auto_ghost_min_latency_ms.is_some() {
                arc.set_auto_ghost_thresholds(
                    args.auto_ghost_min_hits,
                    args.auto_ghost_min_latency_ms,
                );
                tracing::info!(
                    "Auto-ghost thresholds overridden: min_hits={:?}, min_latency_ms={:?}",
                    args.auto_ghost_min_hits,
                    args.auto_ghost_min_latency_ms,
                );
            }
            arc
        }
        Err(e) => {
            tracing::error!("Failed to open database: {e}");
            std::process::exit(1);
        }
    };

    // v0.4 cp 2.2.2: load bearer token if --auth-token configured.
    let expected_token: Arc<Option<String>> = match &args.auth_token {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(s) => {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    tracing::error!(
                        "--auth-token file {} is empty; refusing to start with empty token",
                        path.display()
                    );
                    std::process::exit(1);
                }
                tracing::info!("Bearer-token auth enabled (token file: {})", path.display());
                Arc::new(Some(trimmed))
            }
            Err(e) => {
                tracing::error!("Failed to read --auth-token {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        None => {
            tracing::warn!(
                "No --auth-token set: clients are not authenticated. Fine on a loopback \
                 bind; on any other address this requires --insecure-allow-no-auth."
            );
            Arc::new(None)
        }
    };

    // v1.0: an unauthenticated server must not reach the network by accident.
    // A non-loopback bind with no token is a deliberate act — require the token
    // or the explicit override.
    if refuse_unauthenticated_bind(
        &args.bind,
        expected_token.is_some(),
        args.insecure_allow_no_auth,
    ) {
        tracing::error!(
            "Refusing to bind {} with no authentication. Set --auth-token <file>, bind to \
             loopback (the default 127.0.0.1), or pass --insecure-allow-no-auth to expose an \
             open server on purpose.",
            args.bind
        );
        std::process::exit(1);
    }

    let addr = format!("{}:{}", args.bind, args.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind to {addr}: {e}");
            std::process::exit(1);
        }
    };

    // v0.4 item 3: build TlsAcceptor if both --tls-cert and --tls-key are
    // present. Pass-only-one is an error. None of either = plain TCP with
    // WARN at boot (back-compat with v0.3.x deployments).
    let tls_acceptor: Option<TlsAcceptor> = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => match build_tls_config(cert, key) {
            Ok(cfg) => {
                tracing::info!(
                    "TLS 1.3 enabled (cert={}, key={})",
                    cert.display(),
                    key.display()
                );
                Some(TlsAcceptor::from(Arc::new(cfg)))
            }
            Err(e) => {
                tracing::error!("Failed to load TLS config: {e}");
                std::process::exit(1);
            }
        },
        (None, None) => {
            tracing::warn!(
                "Server listening on plain TCP (no --tls-cert/--tls-key). \
                 Suitable for trusted networks; use TLS for production."
            );
            None
        }
        _ => {
            tracing::error!("--tls-cert and --tls-key must be set together");
            std::process::exit(1);
        }
    };

    tracing::info!("xyzDB server listening on {addr}");

    // V3: Start flush timer for batched durability mode
    if durability == xyzdb_engine::engine::DurabilityMode::Batched {
        let flush_engine = engine.clone();
        let interval_ms = args.batch_interval;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
            loop {
                interval.tick().await;
                if let Err(e) = flush_engine.persist_journal() {
                    // 5a/5b: a periodic-flush fsync failure means the buffered
                    // window is NOT durable. turba.persist() has poisoned the
                    // WAL (commits now fail fast, 3a parity); surface it loudly
                    // and stop the timer — retrying a poisoned WAL could
                    // false-succeed (fsyncgate). Operator must restart.
                    tracing::error!(
                        "Batched durability: WAL fsync FAILED ({e}); WAL poisoned, \
                         writes will fail fast — restart required. Halting flush timer."
                    );
                    break;
                }
            }
        });
        tracing::info!("Batched durability: flushing every {}ms", interval_ms);
    }

    // Graceful shutdown: the accept loop races a shutdown signal. On signal we
    // STOP accepting (break), then — strictly in this order, never in parallel
    // with accept — drain in-flight connections with a bounded timeout, abort any
    // stragglers, flush the engine, and exit. Writes are WAL-durable, so aborting
    // a straggler loses only its in-flight response, not committed data.
    let mut conns: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown_signal() => {
                tracing::info!("Shutdown signal received; no longer accepting connections");
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer_addr)) => {
                    let engine = engine.clone();
                    let token = expected_token.clone();
                    if let Some(acceptor) = &tls_acceptor {
                        let acceptor = acceptor.clone();
                        conns.spawn(async move {
                            match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    xyzdb_server::connection::handle_tls_connection(
                                        engine, tls_stream, peer_addr, token,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    tracing::warn!("TLS handshake failed from {peer_addr}: {e}");
                                }
                            }
                        });
                    } else {
                        conns.spawn(xyzdb_server::connection::handle_connection(
                            engine, stream, peer_addr, token,
                        ));
                    }
                }
                Err(e) => {
                    tracing::error!("Accept error: {e}");
                }
            }
        }
        // Reap finished connection tasks so the JoinSet does not grow unbounded.
        while conns.try_join_next().is_some() {}
    }

    // Strict order after the signal: stop-accept (loop broken) → bounded drain →
    // abort stragglers → flush → exit.
    let inflight = conns.len();
    if inflight > 0 {
        tracing::info!("Draining {inflight} in-flight connection(s) (up to 5s)");
        if tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while conns.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            tracing::warn!("Drain timeout; aborting {} straggler(s)", conns.len());
        }
    }
    conns.shutdown().await; // abort any connection still running past the timeout

    tracing::info!("Sealing memtables + flushing before exit");
    engine.graceful_shutdown();
    tracing::info!("Shutdown complete");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::refuse_unauthenticated_bind;

    #[test]
    fn loopback_defaults_are_allowed_without_auth() {
        for b in ["127.0.0.1", "::1", "localhost", "127.0.0.5"] {
            assert!(
                !refuse_unauthenticated_bind(b, false, false),
                "{b} should be allowed"
            );
        }
    }

    #[test]
    fn public_bind_without_token_is_refused() {
        for b in ["0.0.0.0", "::", "10.0.0.5", "192.168.1.10"] {
            assert!(
                refuse_unauthenticated_bind(b, false, false),
                "{b} should be refused"
            );
        }
    }

    #[test]
    fn a_token_or_the_override_permits_a_public_bind() {
        assert!(!refuse_unauthenticated_bind("0.0.0.0", true, false)); // has token
        assert!(!refuse_unauthenticated_bind("0.0.0.0", false, true)); // explicit override
    }

    #[test]
    fn an_unparseable_bind_is_treated_as_non_loopback() {
        assert!(refuse_unauthenticated_bind("db.internal", false, false));
        assert!(!refuse_unauthenticated_bind("db.internal", true, false));
    }
}
