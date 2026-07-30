use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use crate::config::Config;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::latency::LatencyCollector;
use crate::utils::tcp_client::TcpClient;

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 3: Write Stress");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Unique run ID to avoid lobe/anchor collisions between runs
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        % 1_000_000;

    // ── 3.1 Burst write (4 connections, 30s) ────────────────────────────

    reporter::print_metric("Test 3.1", "Burst write (4 connections, 30s)");

    let burst_passed = match test_burst_write(config, run_id).await {
        Ok((passed, total_ops, per_conn, elapsed_secs)) => {
            let throughput = total_ops as f64 / elapsed_secs;
            reporter::print_metric(
                "  Total ops",
                &format!("{} in {:.2}s", format_num(total_ops), elapsed_secs),
            );
            reporter::print_metric("  Throughput", &format!("{:.0} ops/s", throughput));
            for (i, ops) in per_conn.iter().enumerate() {
                reporter::print_metric(
                    &format!("  Conn {i}"),
                    &format!("{} ops ({:.0} ops/s)", format_num(*ops), *ops as f64 / elapsed_secs),
                );
            }
            reporter::print_result(
                "3.1 Burst write",
                passed,
                &format!("{:.0} ops/s", throughput),
                &format!("{} total ops", format_num(total_ops)),
            );
            results.push(TestResult {
                name: "3.1 Burst write (4 conn, 30s)".into(),
                passed,
                value: format!("{} ops, {:.0} ops/s", format_num(total_ops), throughput),
                expected: "> 0 ops".into(),
                notes: format!("4 connections, {:.2}s", elapsed_secs),
            });
            passed
        }
        Err(e) => {
            errors.push(format!("3.1 Burst write: {e}"));
            reporter::print_result("3.1 Burst write", false, "ERROR", &format!("{e}"));
            results.push(TestResult {
                name: "3.1 Burst write (4 conn, 30s)".into(),
                passed: false,
                value: "ERROR".into(),
                expected: "> 0 ops".into(),
                notes: format!("{e}"),
            });
            false
        }
    };

    reporter::print_separator();

    // ── 3.2 Batch sizing sweep ──────────────────────────────────────────

    reporter::print_metric("Test 3.2", "Batch sizing sweep (~10K records total)");

    let sweep_passed = match test_batch_sizing_sweep(config, run_id).await {
        Ok((passed, sweep_results)) => {
            for (batch_size, count, elapsed_ms, throughput) in &sweep_results {
                reporter::print_metric(
                    &format!("  batch_size={:<4}", batch_size),
                    &format!(
                        "{} records in {:.1}ms ({:.0} rec/s)",
                        format_num(*count as u64),
                        elapsed_ms,
                        throughput,
                    ),
                );
            }
            reporter::print_result(
                "3.2 Batch sizing sweep",
                passed,
                &format!("{} sizes tested", sweep_results.len()),
                "all batches OK",
            );
            results.push(TestResult {
                name: "3.2 Batch sizing sweep".into(),
                passed,
                value: format!("{} sizes tested", sweep_results.len()),
                expected: "all batch sizes succeed".into(),
                notes: sweep_results
                    .iter()
                    .map(|(bs, _, _, tp)| format!("{}={:.0}", bs, tp))
                    .collect::<Vec<_>>()
                    .join(", "),
            });
            passed
        }
        Err(e) => {
            errors.push(format!("3.2 Batch sizing sweep: {e}"));
            reporter::print_result("3.2 Batch sizing sweep", false, "ERROR", &format!("{e}"));
            results.push(TestResult {
                name: "3.2 Batch sizing sweep".into(),
                passed: false,
                value: "ERROR".into(),
                expected: "all batch sizes succeed".into(),
                notes: format!("{e}"),
            });
            false
        }
    };

    reporter::print_separator();

    // ── 3.3 Anchor contention (4 connections) ───────────────────────────

    reporter::print_metric("Test 3.3", "Anchor contention (4 connections, 10K records)");

    let contention_passed = match test_anchor_contention(config, run_id).await {
        Ok((passed, succeeded, failed, elapsed_secs)) => {
            let throughput = succeeded as f64 / elapsed_secs;
            reporter::print_metric(
                "  Results",
                &format!(
                    "{} succeeded, {} failed in {:.2}s ({:.0} ops/s)",
                    format_num(succeeded),
                    format_num(failed),
                    elapsed_secs,
                    throughput,
                ),
            );
            reporter::print_result(
                "3.3 Anchor contention",
                passed,
                &format!("{} ok, {} fail", format_num(succeeded), format_num(failed)),
                &format!("{:.0} ops/s", throughput),
            );
            results.push(TestResult {
                name: "3.3 Anchor contention (4 conn)".into(),
                passed,
                value: format!("{} succeeded, {} failed", format_num(succeeded), format_num(failed)),
                expected: "10,000 succeeded, 0 failed".into(),
                notes: format!("{:.0} ops/s, {:.2}s", throughput, elapsed_secs),
            });
            passed
        }
        Err(e) => {
            errors.push(format!("3.3 Anchor contention: {e}"));
            reporter::print_result("3.3 Anchor contention", false, "ERROR", &format!("{e}"));
            results.push(TestResult {
                name: "3.3 Anchor contention (4 conn)".into(),
                passed: false,
                value: "ERROR".into(),
                expected: "10,000 succeeded, 0 failed".into(),
                notes: format!("{e}"),
            });
            false
        }
    };

    reporter::print_separator();

    // ── 3.4 ON CONFLICT UPDATE under load ───────────────────────────────

    reporter::print_metric("Test 3.4", "ON CONFLICT UPDATE under load (5K records)");

    let upsert_passed = match test_on_conflict_update(config, run_id).await {
        Ok((passed, insert_p50, insert_p99, upsert_p50, upsert_p99, insert_tp, upsert_tp)) => {
            reporter::print_metric(
                "  Insert latency",
                &format!("p50={:.2}ms p99={:.2}ms ({:.0} ops/s)", insert_p50, insert_p99, insert_tp),
            );
            reporter::print_metric(
                "  Upsert latency",
                &format!("p50={:.2}ms p99={:.2}ms ({:.0} ops/s)", upsert_p50, upsert_p99, upsert_tp),
            );
            let ratio = if insert_p50 > 0.0 { upsert_p50 / insert_p50 } else { 0.0 };
            reporter::print_metric("  Upsert/Insert ratio (p50)", &format!("{:.2}x", ratio));
            reporter::print_result(
                "3.4 ON CONFLICT UPDATE",
                passed,
                &format!("upsert p50={:.2}ms", upsert_p50),
                &format!("{:.2}x vs insert", ratio),
            );
            results.push(TestResult {
                name: "3.4 ON CONFLICT UPDATE".into(),
                passed,
                value: format!(
                    "insert p50={:.2}ms, upsert p50={:.2}ms",
                    insert_p50, upsert_p50,
                ),
                expected: "5K upserts succeed".into(),
                notes: format!(
                    "{:.2}x ratio, insert {:.0} ops/s, upsert {:.0} ops/s",
                    ratio, insert_tp, upsert_tp,
                ),
            });
            passed
        }
        Err(e) => {
            errors.push(format!("3.4 ON CONFLICT UPDATE: {e}"));
            reporter::print_result("3.4 ON CONFLICT UPDATE", false, "ERROR", &format!("{e}"));
            results.push(TestResult {
                name: "3.4 ON CONFLICT UPDATE".into(),
                passed: false,
                value: "ERROR".into(),
                expected: "5K upserts succeed".into(),
                notes: format!("{e}"),
            });
            false
        }
    };

    reporter::print_separator();

    // ── 3.5 SET massive ─────────────────────────────────────────────────

    reporter::print_metric("Test 3.5", "SET massive (1,000 codes)");

    let set_passed = match test_set_massive(config).await {
        Ok((passed, count, p50, throughput, elapsed_secs)) => {
            reporter::print_metric(
                "  SET results",
                &format!(
                    "{} SETs in {:.2}s, p50={:.2}ms, {:.0} ops/s",
                    format_num(count as u64),
                    elapsed_secs,
                    p50,
                    throughput,
                ),
            );
            reporter::print_result(
                "3.5 SET massive",
                passed,
                &format!("p50={:.2}ms", p50),
                &format!("{:.0} ops/s", throughput),
            );
            results.push(TestResult {
                name: "3.5 SET massive (1,000 codes)".into(),
                passed,
                value: format!("{} SETs, p50={:.2}ms", format_num(count as u64), p50),
                expected: "> 0 successful SETs".into(),
                notes: format!("{:.0} ops/s, {:.2}s total", throughput, elapsed_secs),
            });
            passed
        }
        Err(e) => {
            errors.push(format!("3.5 SET massive: {e}"));
            reporter::print_result("3.5 SET massive", false, "ERROR", &format!("{e}"));
            results.push(TestResult {
                name: "3.5 SET massive (1,000 codes)".into(),
                passed: false,
                value: "ERROR".into(),
                expected: "> 0 successful SETs".into(),
                notes: format!("{e}"),
            });
            false
        }
    };

    reporter::print_separator();

    // ── 3.6 DELETE + verify ─────────────────────────────────────────────

    reporter::print_metric("Test 3.6", "DELETE + verify (100 records)");

    let delete_passed = match test_delete_and_verify(config, run_id).await {
        Ok(passed) => {
            reporter::print_result(
                "3.6 DELETE + verify",
                passed,
                if passed { "0 after delete" } else { "FAIL" },
                "100 inserted, 100 deleted",
            );
            results.push(TestResult {
                name: "3.6 DELETE + verify".into(),
                passed,
                value: if passed { "SCAN returns 0".into() } else { "SCAN returned > 0".into() },
                expected: "0 records after delete".into(),
                notes: "100 inserted, 100 deleted, verified with SCAN".into(),
            });
            passed
        }
        Err(e) => {
            errors.push(format!("3.6 DELETE + verify: {e}"));
            reporter::print_result("3.6 DELETE + verify", false, "ERROR", &format!("{e}"));
            results.push(TestResult {
                name: "3.6 DELETE + verify".into(),
                passed: false,
                value: "ERROR".into(),
                expected: "0 records after delete".into(),
                notes: format!("{e}"),
            });
            false
        }
    };

    // ── Summary ─────────────────────────────────────────────────────────

    let suite_elapsed = suite_start.elapsed();
    reporter::print_separator();
    reporter::print_metric(
        "Suite 3 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = burst_passed
        && sweep_passed
        && contention_passed
        && upsert_passed
        && set_passed
        && delete_passed;

    Ok(SuiteReport {
        name: "Suite 3: Write Stress".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}

// ─── 3.1 Burst write ────────────────────────────────────────────────────────

/// Returns (passed, total_ops, per_connection_ops, elapsed_secs).
async fn test_burst_write(
    config: &Config,
    run_id: u128,
) -> Result<(bool, u64, Vec<u64>, f64)> {
    const NUM_CONNS: usize = 4;
    const DURATION_SECS: u64 = 30;

    let total_ops = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut handles = Vec::with_capacity(NUM_CONNS);
    let start = Instant::now();

    for conn_id in 0..NUM_CONNS {
        let host = config.host.clone();
        let port = config.port;
        let total_ops = Arc::clone(&total_ops);
        let stop = Arc::clone(&stop);

        let handle = tokio::spawn(async move {
            let mut client = TcpClient::connect(&host, port)
                .await
                .context(format!("3.1: connect conn {conn_id}"))?;

            let mut seq: u64 = 0;
            let mut conn_ops: u64 = 0;

            while !stop.load(Ordering::Relaxed) {
                let q = format!(
                    "PUT {{_type: \"Burst\", seq: {seq}, conn: {conn_id}, data: \"payload_{run_id}\"}} IN \"catalog\"",
                    seq = seq,
                    conn_id = conn_id,
                    run_id = run_id,
                );
                match client.exec(&q).await {
                    Ok(()) => {
                        conn_ops += 1;
                        total_ops.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // Continue on transient errors
                    }
                }
                seq += 1;
            }

            Ok::<u64, anyhow::Error>(conn_ops)
        });
        handles.push(handle);
    }

    // Wait for the burst duration
    tokio::time::sleep(Duration::from_secs(DURATION_SECS)).await;
    stop.store(true, Ordering::Relaxed);

    let elapsed_secs = start.elapsed().as_secs_f64();

    let mut per_conn = Vec::with_capacity(NUM_CONNS);
    for handle in handles {
        let conn_ops = handle
            .await
            .context("3.1: join task")?
            .context("3.1: task error")?;
        per_conn.push(conn_ops);
    }

    let total = total_ops.load(Ordering::Relaxed);
    let passed = total > 0;
    Ok((passed, total, per_conn, elapsed_secs))
}

// ─── 3.2 Batch sizing sweep ─────────────────────────────────────────────────

/// Returns (passed, vec of (batch_size, record_count, elapsed_ms, throughput)).
async fn test_batch_sizing_sweep(
    config: &Config,
    run_id: u128,
) -> Result<(bool, Vec<(usize, usize, f64, f64)>)> {
    let mut client = TcpClient::connect(&config.host, config.port)
        .await
        .context("3.2: connect")?;

    let lobe = format!("bsweep_{run_id}");
    client
        .exec(&format!("LOBE \"{lobe}\""))
        .await
        .context("3.2: create LOBE")?;

    let batch_sizes: &[usize] = &[1, 10, 36, 100, 500];
    let total_target = 10_000usize;
    let mut sweep_results = Vec::with_capacity(batch_sizes.len());
    let all_ok = true;
    let mut global_n: usize = 0;

    for &batch_size in batch_sizes {
        let num_batches = total_target / batch_sizes.len() / batch_size;
        let num_batches = num_batches.max(1);
        let mut records_inserted: usize = 0;

        let op_start = Instant::now();

        for batch_idx in 0..num_batches {
            let mut records = String::new();
            for j in 0..batch_size {
                if j > 0 {
                    records.push_str(", ");
                }
                records.push_str(&format!(
                    "{{_type: \"Sweep\", n: {n}, batch: {batch_idx}}}",
                    n = global_n,
                    batch_idx = batch_idx,
                ));
                global_n += 1;
            }

            let q = format!("PUT BATCH IN \"{lobe}\" [{records}]");
            match client.exec(&q).await {
                Ok(()) => {
                    records_inserted += batch_size;
                }
                Err(e) => {
                    return Err(e).context(format!(
                        "3.2: PUT BATCH size={batch_size} batch_idx={batch_idx}"
                    ));
                }
            }
        }

        let elapsed = op_start.elapsed();
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let throughput = if elapsed.as_secs_f64() > 0.0 {
            records_inserted as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        sweep_results.push((batch_size, records_inserted, elapsed_ms, throughput));
    }

    Ok((all_ok, sweep_results))
}

// ─── 3.3 Anchor contention ──────────────────────────────────────────────────

/// Returns (passed, succeeded_count, failed_count, elapsed_secs).
async fn test_anchor_contention(
    config: &Config,
    run_id: u128,
) -> Result<(bool, u64, u64, f64)> {
    const NUM_CONNS: usize = 4;
    const RECORDS_PER_CONN: u64 = 2_500;

    let lobe = format!("contention_{run_id}");

    // Setup: create lobe and anchor from a single connection
    let mut setup_client = TcpClient::connect(&config.host, config.port)
        .await
        .context("3.3: setup connect")?;
    setup_client
        .exec(&format!("LOBE \"{lobe}\""))
        .await
        .context("3.3: create LOBE")?;
    setup_client
        .exec(&format!("ANCHOR \"code\" UNIQUE IN \"{lobe}\""))
        .await
        .context("3.3: create ANCHOR")?;
    drop(setup_client);

    let succeeded = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(NUM_CONNS);
    let start = Instant::now();

    for task_id in 0..NUM_CONNS {
        let host = config.host.clone();
        let port = config.port;
        let lobe = lobe.clone();
        let succeeded = Arc::clone(&succeeded);
        let failed = Arc::clone(&failed);

        let handle = tokio::spawn(async move {
            let mut client = TcpClient::connect(&host, port)
                .await
                .context(format!("3.3: connect task {task_id}"))?;

            let range_start = task_id as u64 * RECORDS_PER_CONN;
            let range_end = range_start + RECORDS_PER_CONN;

            for i in range_start..range_end {
                let code = format!("COM-C{task_id}-{i:04}");
                let q = format!(
                    "PUT {{_type: \"Contention\", code: \"{code}\", task: {task_id}}} IN \"{lobe}\""
                );
                match client.exec(&q).await {
                    Ok(()) => {
                        succeeded.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        });
        handles.push(handle);
    }

    for handle in handles {
        handle
            .await
            .context("3.3: join task")?
            .context("3.3: task error")?;
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    let total_succeeded = succeeded.load(Ordering::Relaxed);
    let total_failed = failed.load(Ordering::Relaxed);

    // All should succeed since ranges don't overlap
    let passed = total_succeeded == (NUM_CONNS as u64 * RECORDS_PER_CONN) && total_failed == 0;

    Ok((passed, total_succeeded, total_failed, elapsed_secs))
}

// ─── 3.4 ON CONFLICT UPDATE under load ──────────────────────────────────────

/// Returns (passed, insert_p50, insert_p99, upsert_p50, upsert_p99, insert_throughput, upsert_throughput).
async fn test_on_conflict_update(
    config: &Config,
    run_id: u128,
) -> Result<(bool, f64, f64, f64, f64, f64, f64)> {
    const COUNT: usize = 5_000;

    let mut client = TcpClient::connect(&config.host, config.port)
        .await
        .context("3.4: connect")?;

    let lobe = format!("upsert_{run_id}");
    client
        .exec(&format!("LOBE \"{lobe}\""))
        .await
        .context("3.4: create LOBE")?;
    client
        .exec(&format!("ANCHOR \"code\" UNIQUE IN \"{lobe}\""))
        .await
        .context("3.4: create ANCHOR")?;

    // Phase 1: Initial inserts
    let mut insert_latency = LatencyCollector::with_capacity(COUNT);

    for i in 0..COUNT {
        let code = format!("UPS-{run_id}-{i:05}");
        let q = format!(
            "PUT {{code: \"{code}\", version: 1, data: \"original\"}} IN \"{lobe}\""
        );
        let op_start = Instant::now();
        client.exec(&q).await.with_context(|| format!("3.4: insert {i}"))?;
        insert_latency.record(op_start.elapsed());
    }

    let insert_p = insert_latency.percentiles();
    let insert_tp = insert_latency.throughput();

    // Phase 2: Upserts with ON CONFLICT UPDATE
    let mut upsert_latency = LatencyCollector::with_capacity(COUNT);

    for i in 0..COUNT {
        let code = format!("UPS-{run_id}-{i:05}");
        let q = format!(
            "PUT {{code: \"{code}\", version: 2, data: \"updated\"}} IN \"{lobe}\" ON CONFLICT UPDATE"
        );
        let op_start = Instant::now();
        client.exec(&q).await.with_context(|| format!("3.4: upsert {i}"))?;
        upsert_latency.record(op_start.elapsed());
    }

    let upsert_p = upsert_latency.percentiles();
    let upsert_tp = upsert_latency.throughput();

    let passed = true; // Both phases completed without error
    Ok((
        passed,
        insert_p.p50_ms(),
        insert_p.p99_ms(),
        upsert_p.p50_ms(),
        upsert_p.p99_ms(),
        insert_tp,
        upsert_tp,
    ))
}

// ─── 3.5 SET massive ────────────────────────────────────────────────────────

/// Returns (passed, count, p50_ms, throughput, elapsed_secs).
async fn test_set_massive(config: &Config) -> Result<(bool, usize, f64, f64, f64)> {
    let codes: Vec<String> = std::fs::read_to_string("/tmp/xyzdb-validate-codes.txt")
        .context("read /tmp/xyzdb-validate-codes.txt (run Suite 1 first)")?
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    let mut rng = StdRng::seed_from_u64(303);
    let mut shuffled = codes;
    shuffled.shuffle(&mut rng);
    let sample: Vec<&String> = shuffled.iter().take(1000).collect();

    let mut client = TcpClient::connect(&config.host, config.port)
        .await
        .context("3.5: connect")?;

    let mut set_latency = LatencyCollector::with_capacity(sample.len());
    let mut success_count: usize = 0;
    let start = Instant::now();

    for code in &sample {
        let q = format!(
            "FIND \"catalog\" WHERE code = \"{code}\" | SET status = \"reviewed\""
        );
        let op_start = Instant::now();
        match client.exec(&q).await {
            Ok(()) => {
                set_latency.record(op_start.elapsed());
                success_count += 1;
            }
            Err(_) => {
                // Record latency even on failure for measurement consistency
                set_latency.record(op_start.elapsed());
            }
        }
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    let set_p = set_latency.percentiles();
    let throughput = set_latency.throughput();

    let passed = success_count > 0;
    Ok((passed, success_count, set_p.p50_ms(), throughput, elapsed_secs))
}

// ─── 3.6 DELETE + verify ────────────────────────────────────────────────────

async fn test_delete_and_verify(config: &Config, run_id: u128) -> Result<bool> {
    const COUNT: usize = 100;

    let mut client = TcpClient::connect(&config.host, config.port)
        .await
        .context("3.6: connect")?;

    let lobe = format!("del_{run_id}");
    client
        .exec(&format!("LOBE \"{lobe}\""))
        .await
        .context("3.6: create LOBE")?;

    // Insert 100 records
    for i in 0..COUNT {
        let q = format!(
            "PUT {{_type: \"Deletable\", seq: {i}, label: \"record_{i}\"}} IN \"{lobe}\""
        );
        client
            .exec(&q)
            .await
            .with_context(|| format!("3.6: insert {i}"))?;
    }

    // Verify they exist
    let pre_scan = client
        .query_text(&format!("SCAN \"{lobe}\""))
        .await
        .context("3.6: pre-delete SCAN")?;
    let pre_count = crate::utils::assertions::count_lids_in_text(&pre_scan);
    reporter::print_metric("  Inserted", &format!("{} records", pre_count));

    // Delete all 100
    for i in 0..COUNT {
        let q = format!(
            "FIND \"{lobe}\" WHERE seq = {i} | DELETE"
        );
        client
            .exec(&q)
            .await
            .with_context(|| format!("3.6: delete seq={i}"))?;
    }

    // Verify SCAN returns 0
    let post_scan = client
        .query_text(&format!("SCAN \"{lobe}\""))
        .await
        .context("3.6: post-delete SCAN")?;
    let post_count = crate::utils::assertions::count_lids_in_text(&post_scan);
    reporter::print_metric("  After delete", &format!("{} records", post_count));

    let passed = post_count == 0;
    if !passed {
        reporter::print_metric(
            "  FAIL detail",
            &format!("expected 0 records after delete, got {post_count}"),
        );
    }

    Ok(passed)
}
