use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::rngs::StdRng;
use rand::prelude::IndexedRandom;
use rand::{SeedableRng, RngExt};

use crate::config::Config;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::tcp_client::TcpClient;

// ── Snapshot for per-minute stats ────────────────────────────────────────────

struct Snapshot {
    minute: u32,
    put_rate: f64,
    pull_rate: f64,
    errors: u64,
}

// ── Public entry point ──────────────────────────────────────────────────────

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 10: Endurance (10 min sustained load)");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ── Load persisted data from Suite 1 ─────────────────────────────────

    let codes: Vec<String> = match std::fs::read_to_string("/tmp/xyzdb-validate-codes.txt") {
        Ok(content) => content.lines().filter(|l| !l.is_empty()).map(String::from).collect(),
        Err(_) => {
            reporter::print_metric("SKIP", "Suite 10 requires /tmp/xyzdb-validate-codes.txt (run Suite 1 first)");
            results.push(TestResult {
                name: "Endurance completed".into(),
                passed: true,
                value: "SKIPPED".into(),
                expected: "OK".into(),
                notes: "data files not found — run Suite 1 first".into(),
            });
            return Ok(SuiteReport {
                name: "Suite 10: Endurance".into(),
                passed: true,
                duration_secs: suite_start.elapsed().as_secs_f64(),
                results,
                errors,
            });
        }
    };

    let project_ids: Vec<String> = match std::fs::read_to_string("/tmp/xyzdb-validate-projects.txt") {
        Ok(content) => content.lines().filter(|l| !l.is_empty()).map(String::from).collect(),
        Err(_) => {
            reporter::print_metric("SKIP", "Suite 10 requires /tmp/xyzdb-validate-projects.txt (run Suite 1 first)");
            results.push(TestResult {
                name: "Endurance completed".into(),
                passed: true,
                value: "SKIPPED".into(),
                expected: "OK".into(),
                notes: "data files not found — run Suite 1 first".into(),
            });
            return Ok(SuiteReport {
                name: "Suite 10: Endurance".into(),
                passed: true,
                duration_secs: suite_start.elapsed().as_secs_f64(),
                results,
                errors,
            });
        }
    };

    reporter::print_metric(
        "Loaded from Suite 1",
        &format!("{} codes, {} projects", format_num(codes.len() as u64), format_num(project_ids.len() as u64)),
    );
    reporter::print_metric("Duration", "600s (10 minutes)");
    reporter::print_metric("Workers", "2 SET (constant dataset) + 2 PULL threads");
    reporter::print_metric("NOTE", "Writes are SETs on existing records — dataset size stays constant");
    reporter::print_separator();

    let codes = Arc::new(codes);
    let _project_ids = Arc::new(project_ids);

    // ── Shared counters ──────────────────────────────────────────────────

    const DURATION_SECS: u64 = 600;

    let put_count = Arc::new(AtomicU64::new(0));
    let pull_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let host = config.host.clone();
    let port = config.port;

    let mut handles = Vec::new();

    // ── Spawn 2 SET workers (constant dataset — update existing records) ─

    for tid in 0..2u64 {
        let stop = Arc::clone(&stop);
        let put_count = Arc::clone(&put_count); // reuse counter name for consistency
        let error_count = Arc::clone(&error_count);
        let codes_w = Arc::clone(&codes);
        let host = host.clone();

        handles.push(tokio::spawn(async move {
            let mut client = match TcpClient::connect(&host, port).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  [SET-{tid}] connect failed: {e}");
                    error_count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            let mut rng = StdRng::seed_from_u64(100 + tid);
            let mut n: u64 = 0;
            let statuses = ["active", "reviewed", "suspended", "in_progress"];

            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let code = match codes_w.choose(&mut rng) {
                    Some(r) => r,
                    None => continue,
                };
                let status = statuses[(n % 4) as usize];
                // SET on existing company record — generates MVCC version but doesn't grow dataset
                let query = format!(
                    "FIND \"catalog\" WHERE code = \"{}\" | SET status = \"{}\", last_review = {}",
                    code, status, n,
                );

                match client.exec(&query).await {
                    Ok(()) => {
                        put_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        error_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    // ── Spawn 2 PULL workers ─────────────────────────────────────────────

    for tid in 0..2u64 {
        let stop = Arc::clone(&stop);
        let pull_count = Arc::clone(&pull_count);
        let error_count = Arc::clone(&error_count);
        let codes = Arc::clone(&codes);
        let host = host.clone();

        handles.push(tokio::spawn(async move {
            let mut client = match TcpClient::connect(&host, port).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  [PULL-{tid}] connect failed: {e}");
                    error_count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            };
            let mut rng = StdRng::seed_from_u64(200 + tid);

            while !stop.load(Ordering::Relaxed) {
                let code = match codes.choose(&mut rng) {
                    Some(r) => r,
                    None => continue,
                };
                let query = format!(
                    "FIND \"catalog\" WHERE code = \"{}\" | PULL depth=1",
                    code,
                );

                match client.exec(&query).await {
                    Ok(()) => {
                        pull_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        error_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    // ── Per-minute snapshot collection ───────────────────────────────────

    let mut snapshots: Vec<Snapshot> = Vec::with_capacity(10);

    // Print table header
    println!();
    reporter::print_metric(
        "  Minute  SET/s      PULL/s     Errors",
        "Notes",
    );
    reporter::print_separator();

    let mut prev_puts: u64 = 0;
    let mut prev_pulls: u64 = 0;
    let mut prev_errors: u64 = 0;

    for minute in 1..=10u32 {
        tokio::time::sleep(Duration::from_secs(60)).await;

        let cur_puts = put_count.load(Ordering::Relaxed);
        let cur_pulls = pull_count.load(Ordering::Relaxed);
        let cur_errors = error_count.load(Ordering::Relaxed);

        let delta_puts = cur_puts - prev_puts;
        let delta_pulls = cur_pulls - prev_pulls;
        let delta_errors = cur_errors - prev_errors;

        let put_rate = delta_puts as f64 / 60.0;
        let pull_rate = delta_pulls as f64 / 60.0;

        snapshots.push(Snapshot {
            minute,
            put_rate,
            pull_rate,
            errors: delta_errors,
        });

        let notes = if delta_errors > 0 {
            format!("{} errors this minute", delta_errors)
        } else {
            String::new()
        };

        reporter::print_metric(
            &format!(
                "  {:<7} {:<10} {:<10} {}",
                minute,
                format_num(put_rate as u64),
                format_num(pull_rate as u64),
                delta_errors,
            ),
            &notes,
        );

        prev_puts = cur_puts;
        prev_pulls = cur_pulls;
        prev_errors = cur_errors;
    }

    // ── Stop all workers ─────────────────────────────────────────────────

    stop.store(true, Ordering::Relaxed);

    for handle in handles {
        let _ = handle.await;
    }

    reporter::print_separator();

    let total_puts = put_count.load(Ordering::Relaxed);
    let total_pulls = pull_count.load(Ordering::Relaxed);
    let total_errors = error_count.load(Ordering::Relaxed);
    let suite_elapsed = suite_start.elapsed();

    reporter::print_metric("Total SETs", &format_num(total_puts));
    reporter::print_metric("Total PULLs", &format_num(total_pulls));
    reporter::print_metric("Total errors", &format_num(total_errors));
    reporter::print_metric(
        "Elapsed",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );
    reporter::print_separator();

    // ── Analysis ─────────────────────────────────────────────────────────

    // 1. Endurance completed (always pass if we got here)
    results.push(TestResult {
        name: "Endurance completed".into(),
        passed: true,
        value: format!(
            "{} SETs + {} PULLs in {:.0}s",
            format_num(total_puts),
            format_num(total_pulls),
            suite_elapsed.as_secs_f64(),
        ),
        expected: "OK (no crash)".into(),
        notes: format!("{} total errors", total_errors),
    });

    reporter::print_result(
        "Endurance completed",
        true,
        &format!("{} SETs + {} PULLs", format_num(total_puts), format_num(total_pulls)),
        &format!("{:.0}s, {} errors", suite_elapsed.as_secs_f64(), total_errors),
    );

    // 2. Error accumulation: pass if total errors < 10
    let error_ok = total_errors < 10;
    results.push(TestResult {
        name: "Error accumulation".into(),
        passed: error_ok,
        value: format!("{} total errors", total_errors),
        expected: "< 10".into(),
        notes: if error_ok {
            "within tolerance".into()
        } else {
            format!("{} errors exceeds threshold of 10", total_errors)
        },
    });

    reporter::print_result(
        "Error accumulation",
        error_ok,
        &format!("{} errors", total_errors),
        if error_ok { "< 10 threshold" } else { "EXCEEDS 10 threshold" },
    );

    if !error_ok {
        errors.push(format!(
            "Error accumulation: {} total errors exceeds threshold of 10",
            total_errors,
        ));
    }

    // 3. Throughput stability: CV of PULL rates across minutes
    let pull_rates: Vec<f64> = snapshots.iter().map(|s| s.pull_rate).collect();
    let (cv, mean_pull, stddev_pull) = coefficient_of_variation(&pull_rates);

    let stability_ok = cv < 0.30;
    results.push(TestResult {
        name: "Throughput stability".into(),
        passed: stability_ok,
        value: format!("CV={:.3} ({:.1}%)", cv, cv * 100.0),
        expected: "CV < 0.30 (30%)".into(),
        notes: format!(
            "PULL mean={:.0}/s, stddev={:.0}/s",
            mean_pull, stddev_pull,
        ),
    });

    reporter::print_result(
        "Throughput stability (CV)",
        stability_ok,
        &format!("{:.3} ({:.1}%)", cv, cv * 100.0),
        if stability_ok { "< 30% variance" } else { "EXCEEDS 30% variance" },
    );

    if !stability_ok {
        errors.push(format!(
            "Throughput stability: CV={:.3} ({:.1}%) exceeds 30% threshold",
            cv, cv * 100.0,
        ));
    }

    // 4. Latency creep: compare minute 1 vs minute 10 PULL rate
    let min1_pull = snapshots.first().map(|s| s.pull_rate).unwrap_or(0.0);
    let min10_pull = snapshots.last().map(|s| s.pull_rate).unwrap_or(0.0);

    let creep_ok = if min1_pull > 0.0 {
        min10_pull > min1_pull * 0.50
    } else {
        // If minute 1 had zero throughput, can't evaluate creep
        true
    };

    let ratio = if min1_pull > 0.0 {
        min10_pull / min1_pull
    } else {
        0.0
    };

    results.push(TestResult {
        name: "Latency creep".into(),
        passed: creep_ok,
        value: format!("min10/min1 = {:.2}x", ratio),
        expected: "> 0.50x (min10 > 50% of min1)".into(),
        notes: format!(
            "min1 PULL={:.0}/s, min10 PULL={:.0}/s",
            min1_pull, min10_pull,
        ),
    });

    reporter::print_result(
        "Latency creep",
        creep_ok,
        &format!("{:.2}x", ratio),
        &format!(
            "min1={:.0}/s, min10={:.0}/s {}",
            min1_pull,
            min10_pull,
            if creep_ok { "OK" } else { "DEGRADATION" },
        ),
    );

    if !creep_ok {
        errors.push(format!(
            "Latency creep: minute 10 PULL rate ({:.0}/s) is < 50% of minute 1 ({:.0}/s)",
            min10_pull, min1_pull,
        ));
    }

    // ── Summary ──────────────────────────────────────────────────────────

    reporter::print_separator();
    reporter::print_metric(
        "Suite 10 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 10: Endurance".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Compute coefficient of variation (stddev / mean) for a slice of values.
/// Returns (cv, mean, stddev). If the slice is empty or mean is zero, returns (0, 0, 0).
fn coefficient_of_variation(values: &[f64]) -> (f64, f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0, 0.0);
    }

    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;

    if mean == 0.0 {
        return (0.0, 0.0, 0.0);
    }

    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();
    let cv = stddev / mean;

    (cv, mean, stddev)
}
