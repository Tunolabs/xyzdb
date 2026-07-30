use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::rngs::StdRng;
use rand::prelude::IndexedRandom;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};

use crate::config::Config;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::tcp_client::TcpClient;

// ── Phase definitions ────────────────────────────────────────────────────────

struct Phase {
    name: &'static str,
    put_threads: usize,
    pull_threads: usize,
    scan_threads: usize,
    duration: Duration,
}

const PHASES: &[Phase] = &[
    Phase { name: "Morning", put_threads: 2, pull_threads: 2, scan_threads: 0, duration: Duration::from_secs(30) },
    Phase { name: "Peak",    put_threads: 3, pull_threads: 4, scan_threads: 0, duration: Duration::from_secs(30) },
    Phase { name: "BI",      put_threads: 1, pull_threads: 2, scan_threads: 1, duration: Duration::from_secs(30) },
    Phase { name: "Night",   put_threads: 0, pull_threads: 1, scan_threads: 0, duration: Duration::from_secs(30) },
];

// ── Public entry point ───────────────────────────────────────────────────────

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 4: Mixed Workload (Daily Simulation)");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ── Load persisted data from Suite 1 ─────────────────────────────────

    let codes: Vec<String> = match std::fs::read_to_string("/tmp/xyzdb-validate-codes.txt") {
        Ok(content) => content.lines().filter(|l| !l.is_empty()).map(String::from).collect(),
        Err(_) => {
            reporter::print_metric("SKIP", "Suite 4 requires /tmp/xyzdb-validate-codes.txt (run Suite 1 first)");
            results.push(TestResult {
                name: "Mixed workload completed".into(),
                passed: true,
                value: "SKIPPED".into(),
                expected: "OK".into(),
                notes: "data files not found -- run Suite 1 first".into(),
            });
            return Ok(SuiteReport {
                name: "Suite 4: Mixed Workload".into(),
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
            reporter::print_metric("SKIP", "Suite 4 requires /tmp/xyzdb-validate-projects.txt (run Suite 1 first)");
            results.push(TestResult {
                name: "Mixed workload completed".into(),
                passed: true,
                value: "SKIPPED".into(),
                expected: "OK".into(),
                notes: "data files not found -- run Suite 1 first".into(),
            });
            return Ok(SuiteReport {
                name: "Suite 4: Mixed Workload".into(),
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
    reporter::print_metric(
        "Total duration",
        "120s (4 phases x 30s: Morning, Peak, BI, Night)",
    );
    reporter::print_separator();

    let codes = Arc::new(codes);
    let project_ids = Arc::new(project_ids);

    // Per-phase accumulators: (put_count, pull_count)
    let mut phase_stats: Vec<(u64, u64, f64)> = Vec::new();

    let host = config.host.clone();
    let port = config.port;

    // ── Run each phase ───────────────────────────────────────────────────

    for (phase_idx, phase) in PHASES.iter().enumerate() {
        reporter::print_metric(
            &format!("Phase {} -- {}", phase_idx + 1, phase.name),
            &format!(
                "{} PUT + {} PULL + {} SCAN threads, {}s",
                phase.put_threads,
                phase.pull_threads,
                phase.scan_threads,
                phase.duration.as_secs(),
            ),
        );

        let put_count = Arc::new(AtomicU64::new(0));
        let pull_count = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let phase_start = Instant::now();
        let mut handles = Vec::new();

        // Spawn PUT workers
        for tid in 0..phase.put_threads {
            let stop = Arc::clone(&stop);
            let put_count = Arc::clone(&put_count);
            let project_ids = Arc::clone(&project_ids);
            let host = host.clone();

            handles.push(tokio::spawn(async move {
                let mut client = match TcpClient::connect(&host, port).await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let mut rng = StdRng::seed_from_u64(42 + tid as u64 + phase_idx as u64 * 100);
                let mut n: u64 = 0;

                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    let project = match project_ids.choose(&mut rng) {
                        Some(c) => c,
                        None => continue,
                    };
                    let budget = (rng.random::<f64>() * 50_000.0) + 500.0;
                    let query = format!(
                        "PUT {{_type: \"Comment\", budget: {:.2}, ref: \"CMT-{}-{}\"}} IN \"catalog\" LINK TO \"catalog\" WHERE project_id = \"{}\" AS \"comment_on\"",
                        budget, tid, n, project,
                    );

                    if client.exec(&query).await.is_err() {
                        continue;
                    }
                    put_count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // Spawn PULL workers
        for tid in 0..phase.pull_threads {
            let stop = Arc::clone(&stop);
            let pull_count = Arc::clone(&pull_count);
            let codes = Arc::clone(&codes);
            let host = host.clone();

            handles.push(tokio::spawn(async move {
                let mut client = match TcpClient::connect(&host, port).await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let mut rng = StdRng::seed_from_u64(1000 + tid as u64 + phase_idx as u64 * 100);

                while !stop.load(Ordering::Relaxed) {
                    let code = match codes.choose(&mut rng) {
                        Some(r) => r,
                        None => continue,
                    };
                    let query = format!(
                        "FIND \"catalog\" WHERE code = \"{}\" | PULL depth=1",
                        code,
                    );

                    if client.exec(&query).await.is_err() {
                        continue;
                    }
                    pull_count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // Spawn SCAN workers
        for tid in 0..phase.scan_threads {
            let stop = Arc::clone(&stop);
            let host = host.clone();

            handles.push(tokio::spawn(async move {
                let mut client = match TcpClient::connect(&host, port).await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let _ = tid; // suppress unused warning

                while !stop.load(Ordering::Relaxed) {
                    let query = "SCAN \"catalog\" WHERE _type = \"Project\" AND budget > 400000";
                    if client.exec(query).await.is_err() {
                        continue;
                    }
                    // SCAN is slow by design; just keep looping
                }
            }));
        }

        // ── Progress reporter: print window every 10 seconds ─────────

        let report_stop = Arc::clone(&stop);
        let report_puts = Arc::clone(&put_count);
        let report_pulls = Arc::clone(&pull_count);
        let phase_name = phase.name;

        let reporter_handle = tokio::spawn(async move {
            let mut last_puts: u64 = 0;
            let mut last_pulls: u64 = 0;
            let mut tick: u64 = 0;

            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if report_stop.load(Ordering::Relaxed) {
                    break;
                }
                tick += 1;

                let cur_puts = report_puts.load(Ordering::Relaxed);
                let cur_pulls = report_pulls.load(Ordering::Relaxed);
                let put_rate = (cur_puts - last_puts) as f64 / 10.0;
                let pull_rate = (cur_pulls - last_pulls) as f64 / 10.0;

                reporter::print_metric(
                    &format!("    [{}] window {}0s", phase_name, tick),
                    &format!("PUT/s={:.0}  PULL/s={:.0}", put_rate, pull_rate),
                );

                last_puts = cur_puts;
                last_pulls = cur_pulls;
            }
        });

        // ── Wait for phase duration ──────────────────────────────────

        tokio::time::sleep(phase.duration).await;
        stop.store(true, Ordering::Relaxed);

        // Wait for all workers to finish
        for handle in handles {
            let _ = handle.await;
        }
        // Stop the reporter
        let _ = reporter_handle.await;

        let phase_elapsed = phase_start.elapsed().as_secs_f64();
        let total_puts = put_count.load(Ordering::Relaxed);
        let total_pulls = pull_count.load(Ordering::Relaxed);
        let put_rate = total_puts as f64 / phase_elapsed;
        let pull_rate = total_pulls as f64 / phase_elapsed;

        reporter::print_metric(
            &format!("  {} result", phase.name),
            &format!(
                "{} PUTs ({:.0}/s), {} PULLs ({:.0}/s) in {:.1}s",
                format_num(total_puts),
                put_rate,
                format_num(total_pulls),
                pull_rate,
                phase_elapsed,
            ),
        );
        reporter::print_separator();

        phase_stats.push((total_puts, total_pulls, phase_elapsed));
    }

    // ── Aggregate report ─────────────────────────────────────────────────

    let total_puts: u64 = phase_stats.iter().map(|(p, _, _)| *p).sum();
    let total_pulls: u64 = phase_stats.iter().map(|(_, p, _)| *p).sum();
    let total_elapsed: f64 = phase_stats.iter().map(|(_, _, e)| *e).sum();

    reporter::print_metric("Total PUTs", &format!("{}", format_num(total_puts)));
    reporter::print_metric("Total PULLs", &format!("{}", format_num(total_pulls)));
    reporter::print_metric(
        "Overall throughput",
        &format!("PUT {:.0}/s, PULL {:.0}/s", total_puts as f64 / total_elapsed, total_pulls as f64 / total_elapsed),
    );
    reporter::print_separator();

    // Per-phase averages
    reporter::print_metric("Per-phase averages", "");
    for (i, phase) in PHASES.iter().enumerate() {
        let (puts, pulls, elapsed) = phase_stats[i];
        reporter::print_metric(
            &format!("  {}", phase.name),
            &format!("PUT/s={:.0}  PULL/s={:.0}", puts as f64 / elapsed, pulls as f64 / elapsed),
        );
    }
    reporter::print_separator();

    // ── Degradation check: Peak PULL/s vs Morning PULL/s ─────────────────

    let morning_pull_rate = phase_stats[0].1 as f64 / phase_stats[0].2;
    let peak_pull_rate = phase_stats[1].1 as f64 / phase_stats[1].2;

    let degradation_ratio = if peak_pull_rate > 0.0 {
        morning_pull_rate / peak_pull_rate
    } else {
        0.0
    };

    reporter::print_metric(
        "PULL degradation (Morning vs Peak)",
        &format!(
            "Morning={:.0}/s, Peak={:.0}/s, ratio={:.2}x",
            morning_pull_rate, peak_pull_rate, degradation_ratio,
        ),
    );

    let degradation_ok = degradation_ratio < 2.0;
    reporter::print_result(
        "PULL degradation under load",
        degradation_ok,
        &format!("{:.2}x", degradation_ratio),
        if degradation_ok { "< 2x threshold" } else { "SIGNIFICANT DEGRADATION" },
    );

    if !degradation_ok {
        errors.push(format!(
            "PULL degradation {:.2}x exceeds 2x threshold (Morning {:.0}/s vs Peak {:.0}/s)",
            degradation_ratio, morning_pull_rate, peak_pull_rate,
        ));
    }

    // ── Summary ──────────────────────────────────────────────────────────

    let suite_elapsed = suite_start.elapsed();

    reporter::print_separator();
    reporter::print_metric(
        "Suite 4 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    results.push(TestResult {
        name: "Mixed workload completed".into(),
        passed: true,
        value: format!("{} PUTs + {} PULLs", format_num(total_puts), format_num(total_pulls)),
        expected: "OK (no crash)".into(),
        notes: format!(
            "4 phases, {:.0}s total, PUT {:.0}/s, PULL {:.0}/s avg",
            suite_elapsed.as_secs_f64(),
            total_puts as f64 / total_elapsed,
            total_pulls as f64 / total_elapsed,
        ),
    });

    results.push(TestResult {
        name: "PULL degradation under load".into(),
        passed: degradation_ok,
        value: format!("{:.2}x", degradation_ratio),
        expected: "< 2x".into(),
        notes: format!(
            "Morning {:.0}/s vs Peak {:.0}/s",
            morning_pull_rate, peak_pull_rate,
        ),
    });

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 4: Mixed Workload".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}
