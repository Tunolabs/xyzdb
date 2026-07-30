use std::time::Instant;

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::RngExt;
use rand::SeedableRng;
use tokio::process::Command;

use crate::config::Config;
use crate::data_generator::DomainGenerator;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::latency::LatencyCollector;
use crate::utils::tcp_client::TcpClient;

const SCALE_POINTS: &[u32] = &[1_000, 5_000, 10_000, 50_000, 100_000];
const SAMPLE_SIZE: usize = 200;

struct ScalePoint {
    companies: u32,
    total_records: u64,
    load_time_secs: f64,
    load_throughput: f64,
    pull_p50_ms: f64,
    pull_p99_ms: f64,
    find_p50_ms: f64,
    put_p50_ms: f64,
}

/// Try connecting to the server in a loop (up to 10 retries, 500ms apart).
async fn wait_for_server(host: &str, port: u16) -> Result<TcpClient> {
    for attempt in 1..=10 {
        match TcpClient::connect(host, port).await {
            Ok(c) => return Ok(c),
            Err(_) if attempt < 10 => {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                return Err(e).context(format!(
                    "could not connect to server on {}:{} after 10 retries",
                    host, port
                ));
            }
        }
    }
    unreachable!()
}

/// Spawn the xyzdb-server process with a custom path and port.
async fn spawn_server(
    server_bin: &std::path::Path,
    db_path: &str,
    port: u16,
) -> Result<tokio::process::Child> {
    let child = Command::new(server_bin)
        .arg("--path")
        .arg(db_path)
        .arg("--port")
        .arg(port.to_string())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "spawn server binary {:?} --path {} --port {}",
                server_bin, db_path, port
            )
        })?;
    Ok(child)
}

/// Format a large number with K/M suffix for the table.
fn format_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 9: Scale Curve");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ── Determine which scale points to run ──────────────────────────────
    let active_points: Vec<u32> = SCALE_POINTS
        .iter()
        .copied()
        .filter(|&p| p <= config.clients)
        .collect();

    if active_points.is_empty() {
        reporter::print_metric("SKIP", "config.clients too small for any scale point");
        return Ok(SuiteReport {
            name: "Suite 9: Scale Curve".into(),
            passed: true,
            duration_secs: suite_start.elapsed().as_secs_f64(),
            results,
            errors,
        });
    }

    reporter::print_metric(
        "Scale points",
        &active_points
            .iter()
            .map(|p| format_num(*p as u64))
            .collect::<Vec<_>>()
            .join(", "),
    );

    // ── Spawn dedicated server instance ──────────────────────────────────
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let db_path = format!("/tmp/xyzdb-scale-{}", run_id);
    let port = config.port + 200;

    // Clean up any leftover directory
    let _ = std::fs::remove_dir_all(&db_path);

    reporter::print_metric("Phase 9.0", "Start dedicated server");

    let mut server = spawn_server(&config.server_bin, &db_path, port)
        .await
        .context("Phase 9.0: spawn scale-curve server")?;

    let mut client = match wait_for_server(&config.host, port).await {
        Ok(c) => c,
        Err(e) => {
            let _ = server.kill().await;
            let _ = server.wait().await;
            let _ = std::fs::remove_dir_all(&db_path);
            return Err(e).context("Phase 9.0: wait for scale-curve server startup");
        }
    };

    reporter::print_metric("  Server started on port", &port.to_string());
    reporter::print_separator();

    // ── Setup schema ─────────────────────────────────────────────────────
    reporter::print_metric("Phase 9.0", "Setup LOBE + ANCHORs");

    client
        .exec(r#"LOBE "catalog""#)
        .await
        .context("9.0: create LOBE catalog")?;

    client
        .exec(r#"ANCHOR "code" UNIQUE IN "catalog""#)
        .await
        .context("9.0: create ANCHOR on code")?;

    client
        .exec(r#"ANCHOR "project_id" UNIQUE IN "catalog""#)
        .await
        .context("9.0: create ANCHOR on project_id")?;

    reporter::print_metric("  Schema", "LOBE catalog + 2 ANCHORs");
    reporter::print_separator();

    // ── Run each scale point incrementally ───────────────────────────────
    let mut datagen = DomainGenerator::new(42);
    let mut scale_data: Vec<ScalePoint> = Vec::new();
    let mut prev_count: u32 = 0;
    // Track a running counter for unique PUT IDs during measurement
    let mut put_counter: u32 = 200_000; // well above max scale point

    for &point in &active_points {
        reporter::print_metric(
            &format!("Scale point: {}", format_num(point as u64)),
            &format!("loading {} -> {} companies", format_num(prev_count as u64), format_num(point as u64)),
        );

        // ── 1. Incremental load ──────────────────────────────────────────
        let load_start = Instant::now();
        let mut records_loaded: u64 = 0;

        for i in prev_count..point {
            let hierarchy = datagen.generate_company_hierarchy(i);

            client
                .exec(&hierarchy.company_query)
                .await
                .with_context(|| format!("PUT company {i} at scale point {point}"))?;

            records_loaded += 1;

            for project in &hierarchy.projects {
                client
                    .exec(&project.project_query)
                    .await
                    .with_context(|| format!("PUT project {} at scale point {point}", project.project_id))?;

                client
                    .exec(&project.task_batch)
                    .await
                    .with_context(|| {
                        format!("PUT BATCH tasks for {} at scale point {point}", project.project_id)
                    })?;

                records_loaded += 1 + project.task_count as u64;
            }
        }

        let load_elapsed = load_start.elapsed().as_secs_f64();
        let load_throughput = if load_elapsed > 0.0 {
            records_loaded as f64 / load_elapsed
        } else {
            0.0
        };

        reporter::print_metric(
            "  Loaded",
            &format!(
                "{} records in {:.2}s ({}/s)",
                format_num(records_loaded),
                load_elapsed,
                format_num(load_throughput as u64),
            ),
        );

        let total_records = DomainGenerator::estimate_records(point);

        // ── 2. Measure PULL P50 ──────────────────────────────────────────
        let mut rng = StdRng::seed_from_u64(point as u64 + 100);
        let mut pull_latency = LatencyCollector::with_capacity(SAMPLE_SIZE);

        for _ in 0..SAMPLE_SIZE {
            let random_id = rng.random_range(0..point);
            let code = format!("COM-{random_id:07}");
            let q = format!(
                r#"FIND "catalog" WHERE code = "{}" | PULL depth=1"#,
                code
            );
            let op_start = Instant::now();
            let _result = client
                .query_bin(&q)
                .await
                .with_context(|| format!("PULL at scale point {point}"))?;
            pull_latency.record(op_start.elapsed());
        }

        let pull_p = pull_latency.percentiles();

        // ── 3. Measure FIND P50 ──────────────────────────────────────────
        let mut find_latency = LatencyCollector::with_capacity(SAMPLE_SIZE);

        for _ in 0..SAMPLE_SIZE {
            let random_id = rng.random_range(0..point);
            let code = format!("COM-{random_id:07}");
            let q = format!(r#"FIND "catalog" WHERE code = "{}""#, code);
            let op_start = Instant::now();
            let _result = client
                .query_bin(&q)
                .await
                .with_context(|| format!("FIND at scale point {point}"))?;
            find_latency.record(op_start.elapsed());
        }

        let find_p = find_latency.percentiles();

        // ── 4. Measure PUT P50 ───────────────────────────────────────────
        let mut put_latency = LatencyCollector::with_capacity(SAMPLE_SIZE);

        for _ in 0..SAMPLE_SIZE {
            let unique_id = put_counter;
            put_counter += 1;
            let code = format!("COM-X{unique_id:07}");
            let q = format!(
                r#"PUT {{_type: "Company", code: "{code}", name: "ScaleTest", description: "Bench", founded: @"1990-01-01", email: "bench{unique_id}@test.com", phone: "0000000000", address: "Test", suite: "1", district: "Test", zip: "00000", region: "Test", state: "Test", country: "US", created_at: @"2026-01-01", status: "active", risk_level: "low", monthly_revenue: 50000, industry: "Technology"}} IN "catalog""#
            );
            let op_start = Instant::now();
            client
                .exec(&q)
                .await
                .with_context(|| format!("PUT at scale point {point}"))?;
            put_latency.record(op_start.elapsed());
        }

        let put_p = put_latency.percentiles();

        reporter::print_metric(
            "  PULL P50 / P99",
            &format!("{:.3}ms / {:.3}ms", pull_p.p50_ms(), pull_p.p99_ms()),
        );
        reporter::print_metric(
            "  FIND P50",
            &format!("{:.3}ms", find_p.p50_ms()),
        );
        reporter::print_metric(
            "  PUT P50",
            &format!("{:.3}ms", put_p.p50_ms()),
        );
        reporter::print_separator();

        scale_data.push(ScalePoint {
            companies: point,
            total_records,
            load_time_secs: load_elapsed,
            load_throughput,
            pull_p50_ms: pull_p.p50_ms(),
            pull_p99_ms: pull_p.p99_ms(),
            find_p50_ms: find_p.p50_ms(),
            put_p50_ms: put_p.p50_ms(),
        });

        prev_count = point;
    }

    // ── Print degradation table ──────────────────────────────────────────
    println!();
    println!("  Scale Curve — Performance vs Dataset Size");
    println!("  ───────────────────────────────────────────────────────────────────────────");
    println!(
        "  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}  {:>10}",
        "Companies", "Records", "Load/s", "PULL P50", "FIND P50", "PUT P50"
    );
    println!("  ───────────────────────────────────────────────────────────────────────────");

    for sp in &scale_data {
        println!(
            "  {:>10}  {:>10}  {:>10}  {:>9.3}ms  {:>9.3}ms  {:>9.3}ms",
            format_num(sp.companies as u64),
            format_compact(sp.total_records),
            format_num(sp.load_throughput as u64),
            sp.pull_p50_ms,
            sp.find_p50_ms,
            sp.put_p50_ms,
        );
    }

    println!("  ───────────────────────────────────────────────────────────────────────────");

    // ── Compute degradation ──────────────────────────────────────────────
    let first = scale_data.first();
    let last = scale_data.last();

    let pull_degradation = match (first, last) {
        (Some(f), Some(l)) if f.pull_p50_ms > 0.0 => l.pull_p50_ms / f.pull_p50_ms,
        _ => 1.0,
    };
    let find_degradation = match (first, last) {
        (Some(f), Some(l)) if f.find_p50_ms > 0.0 => l.find_p50_ms / f.find_p50_ms,
        _ => 1.0,
    };
    let put_degradation = match (first, last) {
        (Some(f), Some(l)) if f.put_p50_ms > 0.0 => l.put_p50_ms / f.put_p50_ms,
        _ => 1.0,
    };

    println!(
        "  Degradation {}->{}:",
        first.map(|f| format_num(f.companies as u64)).unwrap_or_default(),
        last.map(|l| format_num(l.companies as u64)).unwrap_or_default(),
    );
    println!(
        "    PULL: {:.2}x    FIND: {:.2}x    PUT: {:.2}x",
        pull_degradation, find_degradation, put_degradation,
    );
    println!();

    // ── Test results ─────────────────────────────────────────────────────
    let all_points_measured = scale_data.len() == active_points.len();

    results.push(TestResult {
        name: "Scale curve completed".into(),
        passed: all_points_measured,
        value: format!("{}/{} points", scale_data.len(), active_points.len()),
        expected: format!("{} points", active_points.len()),
        notes: format!(
            "{}..{} companies",
            active_points.first().map(|p| format_num(*p as u64)).unwrap_or_default(),
            active_points.last().map(|p| format_num(*p as u64)).unwrap_or_default(),
        ),
    });

    let pull_ok = pull_degradation < 5.0;
    results.push(TestResult {
        name: "PULL degradation".into(),
        passed: pull_ok,
        value: format!("{:.2}x", pull_degradation),
        expected: "< 5.00x".into(),
        notes: format!(
            "{:.3}ms -> {:.3}ms",
            first.map(|f| f.pull_p50_ms).unwrap_or(0.0),
            last.map(|l| l.pull_p50_ms).unwrap_or(0.0),
        ),
    });

    if !pull_ok {
        errors.push(format!(
            "PULL P50 degradation {:.2}x exceeds 5x threshold",
            pull_degradation
        ));
    }

    let find_ok = find_degradation < 3.0;
    results.push(TestResult {
        name: "FIND degradation".into(),
        passed: find_ok,
        value: format!("{:.2}x", find_degradation),
        expected: "< 3.00x".into(),
        notes: format!(
            "{:.3}ms -> {:.3}ms",
            first.map(|f| f.find_p50_ms).unwrap_or(0.0),
            last.map(|l| l.find_p50_ms).unwrap_or(0.0),
        ),
    });

    if !find_ok {
        errors.push(format!(
            "FIND P50 degradation {:.2}x exceeds 3x threshold",
            find_degradation
        ));
    }

    // ── Cleanup ──────────────────────────────────────────────────────────
    reporter::print_metric("Cleanup", "Stopping server and removing DB");

    drop(client);

    let _ = server.kill().await;
    let _ = server.wait().await;
    let _ = std::fs::remove_dir_all(&db_path);

    reporter::print_metric("  Server stopped", "OK");
    reporter::print_metric("  DB path removed", &db_path);

    // ── Summary ──────────────────────────────────────────────────────────
    let suite_elapsed = suite_start.elapsed();
    reporter::print_separator();
    reporter::print_metric(
        "Suite 9 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 9: Scale Curve".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}
