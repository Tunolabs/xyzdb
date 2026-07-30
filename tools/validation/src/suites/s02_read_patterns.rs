use std::time::Instant;

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use crate::config::Config;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::assertions;
use crate::utils::latency::LatencyCollector;
use crate::utils::tcp_client::TcpClient;

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 2: Read Patterns");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let mut client = TcpClient::connect(&config.host, config.port)
        .await
        .context("Suite 2: connect to xyzdb-server")?;

    // ── Load persisted data from Suite 1 ─────────────────────────────────

    let codes: Vec<String> = std::fs::read_to_string("/tmp/xyzdb-validate-codes.txt")
        .context("read /tmp/xyzdb-validate-codes.txt (run Suite 1 first)")?
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    let _project_ids: Vec<String> = std::fs::read_to_string("/tmp/xyzdb-validate-projects.txt")
        .context("read /tmp/xyzdb-validate-projects.txt (run Suite 1 first)")?
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    reporter::print_metric(
        "Loaded from Suite 1",
        &format!("{} codes, {} projects", format_num(codes.len() as u64), format_num(_project_ids.len() as u64)),
    );
    reporter::print_separator();

    let mut rng = StdRng::seed_from_u64(42);

    // ── 2.1 FIND by anchor ──────────────────────────────────────────────

    reporter::print_metric("Test 2.1", "FIND by anchor (code) - 1,000 random lookups");

    let mut shuffled_codes = codes.clone();
    shuffled_codes.shuffle(&mut rng);
    let sample_codes: Vec<&String> = shuffled_codes.iter().take(1050).collect();

    // Warm up with 50
    for code in &sample_codes[..50] {
        let q = format!(r#"FIND "catalog" WHERE code = "{}""#, code);
        let _ = client.query_bin(&q).await.context("warmup FIND by code")?;
    }

    let test_codes = &sample_codes[50..1050];
    let mut find_latency = LatencyCollector::with_capacity(1000);

    for code in test_codes {
        let q = format!(r#"FIND "catalog" WHERE code = "{}""#, code);
        let op_start = Instant::now();
        let result = client.query_bin(&q).await.context("FIND by code")?;
        find_latency.record(op_start.elapsed());

        let count = assertions::record_count(&result);
        if count == 0 {
            errors.push(format!("FIND by code={} returned 0 records", code));
        }
    }

    let mut find_latency = find_latency;
    let find_p = find_latency.percentiles();
    let find_throughput = find_latency.throughput();

    reporter::print_metric(
        "  Latency p50/p99",
        &format!("{:.2}ms / {:.2}ms", find_p.p50_ms(), find_p.p99_ms()),
    );
    reporter::print_metric("  Throughput", &format!("{:.0} ops/s", find_throughput));

    let find_passed = find_p.p99_ms() < 100.0;
    reporter::print_result(
        "2.1 FIND by anchor",
        find_passed,
        &format!("p99={:.2}ms", find_p.p99_ms()),
        &format!("{:.0} ops/s", find_throughput),
    );

    results.push(TestResult {
        name: "2.1 FIND by anchor (code)".into(),
        passed: find_passed,
        value: format!("p50={:.2}ms p99={:.2}ms", find_p.p50_ms(), find_p.p99_ms()),
        expected: "p99 < 100ms".into(),
        notes: format!("1,000 lookups, {:.0} ops/s", find_throughput),
    });

    reporter::print_separator();

    // ── 2.2 PULL complete ───────────────────────────────────────────────

    reporter::print_metric("Test 2.2", "PULL complete (depth=1) - 1,000 random");

    let mut shuffled = codes.clone();
    shuffled.shuffle(&mut rng);
    let pull_codes: Vec<&String> = shuffled.iter().take(1000).collect();
    let mut pull_latency = LatencyCollector::with_capacity(1000);
    let mut total_pull_records: u64 = 0;

    for code in &pull_codes {
        let q = format!(r#"FIND "catalog" WHERE code = "{}" | PULL depth=1"#, code);
        let op_start = Instant::now();
        let result = client.query_bin(&q).await.context("PULL complete")?;
        pull_latency.record(op_start.elapsed());

        let count = assertions::record_count(&result);
        total_pull_records += count as u64;
    }

    let mut pull_latency = pull_latency;
    let pull_p = pull_latency.percentiles();
    let pull_throughput = pull_latency.throughput();
    let avg_records_per_pull = total_pull_records as f64 / 1000.0;

    reporter::print_metric(
        "  Latency p50/p99",
        &format!("{:.2}ms / {:.2}ms", pull_p.p50_ms(), pull_p.p99_ms()),
    );
    reporter::print_metric("  Throughput", &format!("{:.0} ops/s", pull_throughput));
    reporter::print_metric("  Avg records/pull", &format!("{:.1}", avg_records_per_pull));

    let pull_passed = avg_records_per_pull > 1.0;
    reporter::print_result(
        "2.2 PULL complete",
        pull_passed,
        &format!("p99={:.2}ms, avg={:.1} rec", pull_p.p99_ms(), avg_records_per_pull),
        &format!("{:.0} ops/s", pull_throughput),
    );

    if !pull_passed {
        errors.push(format!("PULL avg records/pull={:.1}, expected > 1.0", avg_records_per_pull));
    }

    results.push(TestResult {
        name: "2.2 PULL complete (depth=1)".into(),
        passed: pull_passed,
        value: format!("p50={:.2}ms p99={:.2}ms", pull_p.p50_ms(), pull_p.p99_ms()),
        expected: "avg records > 1".into(),
        notes: format!("avg {:.1} rec/pull, {:.0} ops/s", avg_records_per_pull, pull_throughput),
    });

    reporter::print_separator();

    // ── 2.3 PULL only=Task ───────────────────────────────────────

    reporter::print_metric("Test 2.3", "PULL only=Task - 100 lookups");

    let mut shuffled2 = codes.clone();
    shuffled2.shuffle(&mut rng);
    let filter_codes: Vec<&String> = shuffled2.iter().take(100).collect();
    let mut filtered_total: u64 = 0;
    let mut full_total: u64 = 0;
    let mut filter_latency = LatencyCollector::with_capacity(100);

    for code in &filter_codes {
        // Filtered PULL
        let q_filtered = format!(
            r#"FIND "catalog" WHERE code = "{}" | PULL depth=1 only=Task"#,
            code
        );
        let op_start = Instant::now();
        let result_filtered = client.query_bin(&q_filtered).await.context("PULL only=Task")?;
        filter_latency.record(op_start.elapsed());
        filtered_total += assertions::record_count(&result_filtered) as u64;

        // Full PULL for comparison
        let q_full = format!(r#"FIND "catalog" WHERE code = "{}" | PULL depth=1"#, code);
        let result_full = client.query_bin(&q_full).await.context("PULL full for comparison")?;
        full_total += assertions::record_count(&result_full) as u64;
    }

    let mut filter_latency = filter_latency;
    let filter_p = filter_latency.percentiles();

    let filter_passed = filtered_total <= full_total;
    reporter::print_metric(
        "  Filtered records",
        &format!("{} (only=Task) vs {} (full)", format_num(filtered_total), format_num(full_total)),
    );
    reporter::print_metric(
        "  Latency p50/p99",
        &format!("{:.2}ms / {:.2}ms", filter_p.p50_ms(), filter_p.p99_ms()),
    );

    reporter::print_result(
        "2.3 PULL only=Task",
        filter_passed,
        &format!("{} vs {} full", format_num(filtered_total), format_num(full_total)),
        if filter_passed { "filtered <= full" } else { "UNEXPECTED: filtered > full" },
    );

    if !filter_passed {
        errors.push("PULL only=Task returned more records than full PULL".into());
    }

    results.push(TestResult {
        name: "2.3 PULL only=Task".into(),
        passed: filter_passed,
        value: format!("{} filtered / {} full", format_num(filtered_total), format_num(full_total)),
        expected: "filtered <= full".into(),
        notes: format!("p99={:.2}ms", filter_p.p99_ms()),
    });

    reporter::print_separator();

    // ── 2.4 SCAN selective ──────────────────────────────────────────────

    reporter::print_metric("Test 2.4", "SCAN selective - 3 scans (text protocol)");

    let scans = [
        (
            r#"SCAN "catalog" WHERE _type = "Project" AND budget > 400000"#,
            "Project budget > 400K",
        ),
        (
            r#"SCAN "catalog" WHERE _type = "Task" AND status = "overdue""#,
            "Task overdue",
        ),
        (
            r#"SCAN "catalog" WHERE _type = "Company" AND region = "US-West""#,
            "Company US-West",
        ),
    ];

    let mut scan_all_passed = true;

    for (query, label) in &scans {
        let op_start = Instant::now();
        let text = client.query_text(query).await.context("SCAN selective")?;
        let elapsed = op_start.elapsed();

        let lid_count = assertions::count_lids_in_text(&text);

        reporter::print_metric(
            &format!("  {}", label),
            &format!("{} LIDs in {:.1}ms", format_num(lid_count as u64), elapsed.as_secs_f64() * 1000.0),
        );

        let scan_ok = lid_count > 0;
        if !scan_ok {
            scan_all_passed = false;
            errors.push(format!("SCAN '{}' returned 0 LIDs", label));
        }
    }

    reporter::print_result(
        "2.4 SCAN selective",
        scan_all_passed,
        if scan_all_passed { "3/3 returned results" } else { "some returned 0" },
        "text protocol",
    );

    results.push(TestResult {
        name: "2.4 SCAN selective".into(),
        passed: scan_all_passed,
        value: if scan_all_passed { "3/3 scans OK".into() } else { "some scans returned 0".into() },
        expected: "all scans return > 0 LIDs".into(),
        notes: "text protocol, 3 selective scans".into(),
    });

    reporter::print_separator();

    // ── 2.5 SCAN + AGGREGATE ────────────────────────────────────────────

    reporter::print_metric("Test 2.5", "SCAN + AGGREGATE - 2 aggregate queries");

    // Aggregate 1: Tasks blocked
    let agg1_start = Instant::now();
    let agg1_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Task" AND status = "blocked" | AGGREGATE count(), sum(budget_total)"#)
        .await
        .context("AGGREGATE tasks blocked")?;
    let agg1_elapsed = agg1_start.elapsed();

    let agg1_count = assertions::get_aggregate_int(&agg1_result, "count").unwrap_or(0);
    let agg1_sum = assertions::get_aggregate_float(&agg1_result, "sum_budget_total").unwrap_or(0.0);
    let agg1_ok = agg1_count > 0;

    reporter::print_metric(
        "  Task blocked",
        &format!("count={}, sum={:.2} in {:.1}ms", format_num(agg1_count as u64), agg1_sum, agg1_elapsed.as_secs_f64() * 1000.0),
    );

    // Aggregate 2: Projects
    let agg2_start = Instant::now();
    let agg2_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Project" | AGGREGATE count(), sum(budget), min(budget), max(budget)"#)
        .await
        .context("AGGREGATE projects")?;
    let agg2_elapsed = agg2_start.elapsed();

    let agg2_count = assertions::get_aggregate_int(&agg2_result, "count").unwrap_or(0);
    let agg2_sum = assertions::get_aggregate_float(&agg2_result, "sum_budget").unwrap_or(0.0);
    let agg2_min = assertions::get_aggregate_float(&agg2_result, "min_budget").unwrap_or(0.0);
    let agg2_max = assertions::get_aggregate_float(&agg2_result, "max_budget").unwrap_or(0.0);
    let agg2_ok = agg2_count > 0;

    reporter::print_metric(
        "  Project aggregation",
        &format!(
            "count={}, sum={:.2}, min={:.2}, max={:.2} in {:.1}ms",
            format_num(agg2_count as u64), agg2_sum, agg2_min, agg2_max,
            agg2_elapsed.as_secs_f64() * 1000.0,
        ),
    );

    let agg_passed = agg1_ok && agg2_ok;
    reporter::print_result(
        "2.5 SCAN + AGGREGATE",
        agg_passed,
        &format!("counts: {}, {}", format_num(agg1_count as u64), format_num(agg2_count as u64)),
        if agg_passed { "both > 0" } else { "UNEXPECTED zero count" },
    );

    if !agg_passed {
        if !agg1_ok {
            errors.push("AGGREGATE tasks blocked returned count=0".into());
        }
        if !agg2_ok {
            errors.push("AGGREGATE projects returned count=0".into());
        }
    }

    results.push(TestResult {
        name: "2.5 SCAN + AGGREGATE".into(),
        passed: agg_passed,
        value: format!("task_count={}, project_count={}", format_num(agg1_count as u64), format_num(agg2_count as u64)),
        expected: "counts > 0".into(),
        notes: format!(
            "project sum={:.2}, min={:.2}, max={:.2}",
            agg2_sum, agg2_min, agg2_max,
        ),
    });

    reporter::print_separator();

    // ── 2.6 FIND by LID ────────────────────────────────────────────────

    reporter::print_metric("Test 2.6", "FIND by LID - 100 lookups");

    let mut shuffled3 = codes.clone();
    shuffled3.shuffle(&mut rng);
    let lid_codes: Vec<&String> = shuffled3.iter().take(100).collect();
    let mut lid_latency = LatencyCollector::with_capacity(100);
    let mut lid_found_count: u32 = 0;

    for code in &lid_codes {
        // First, FIND by code to get a record with a LID (text protocol to parse LID)
        let q = format!(r#"FIND "catalog" WHERE code = "{}""#, code);
        let text = client.query_text(&q).await.context("FIND by code for LID extraction")?;

        // Parse the first "LID: XXXX" from response (format: "| LID: XXXX|")
        let lid = text
            .lines()
            .find_map(|line| {
                let trimmed = line.trim().trim_start_matches('│').trim();
                if trimmed.starts_with("LID:") {
                    let lid_str = trimmed.trim_start_matches("LID:").trim().trim_end_matches('│').trim();
                    Some(lid_str.to_string())
                } else {
                    None
                }
            });

        let lid = match lid {
            Some(l) => l,
            None => {
                errors.push(format!("Could not extract LID from FIND code={}", code));
                continue;
            }
        };

        // Now FIND by LID
        let lid_q = format!(r#"FIND LID("{}")"#, lid);
        let op_start = Instant::now();
        let result = client.query_bin(&lid_q).await.context("FIND by LID")?;
        lid_latency.record(op_start.elapsed());

        if assertions::record_count(&result) > 0 {
            lid_found_count += 1;
        }
    }

    let mut lid_latency = lid_latency;
    let lid_p = lid_latency.percentiles();
    let lid_throughput = lid_latency.throughput();

    reporter::print_metric(
        "  Latency p50/p99",
        &format!("{:.2}ms / {:.2}ms", lid_p.p50_ms(), lid_p.p99_ms()),
    );
    reporter::print_metric("  Throughput", &format!("{:.0} ops/s", lid_throughput));
    reporter::print_metric("  Found", &format!("{}/100", lid_found_count));

    let lid_passed = lid_found_count > 0;
    reporter::print_result(
        "2.6 FIND by LID",
        lid_passed,
        &format!("p99={:.2}ms, {}/{}", lid_p.p99_ms(), lid_found_count, lid_latency.count()),
        &format!("{:.0} ops/s", lid_throughput),
    );

    if !lid_passed {
        errors.push("FIND by LID found 0 records".into());
    }

    results.push(TestResult {
        name: "2.6 FIND by LID".into(),
        passed: lid_passed,
        value: format!("p50={:.2}ms p99={:.2}ms", lid_p.p50_ms(), lid_p.p99_ms()),
        expected: "found > 0".into(),
        notes: format!("{}/{} found, {:.0} ops/s", lid_found_count, lid_latency.count(), lid_throughput),
    });

    reporter::print_separator();

    // ── 2.7 FIND non-anchor fallback ────────────────────────────────────

    reporter::print_metric("Test 2.7", "FIND non-anchor fallback (name) - 10 queries");

    let names = [
        "Ivan", "Maria", "Carlos", "Ana", "Pedro",
        "Luis", "Sofia", "Diego", "Elena", "Jorge",
    ];

    let mut fallback_latency = LatencyCollector::with_capacity(10);
    let mut fallback_total_lids: usize = 0;

    for name in &names {
        let q = format!(r#"FIND "catalog" WHERE name = "{}""#, name);
        let op_start = Instant::now();
        let text = client.query_text(&q).await.context("FIND non-anchor fallback")?;
        fallback_latency.record(op_start.elapsed());

        let lid_count = assertions::count_lids_in_text(&text);
        fallback_total_lids += lid_count;
    }

    let mut fallback_latency = fallback_latency;
    let fallback_p = fallback_latency.percentiles();

    reporter::print_metric(
        "  Latency p50/p99",
        &format!("{:.2}ms / {:.2}ms", fallback_p.p50_ms(), fallback_p.p99_ms()),
    );
    reporter::print_metric("  Total LIDs found", &format!("{}", fallback_total_lids));

    // Non-anchor should be significantly slower than anchor-based FIND
    let ratio = if find_p.p50_ms() > 0.0 {
        fallback_p.p50_ms() / find_p.p50_ms()
    } else {
        0.0
    };

    reporter::print_metric(
        "  Slowdown vs anchor",
        &format!("{:.1}x (p50: {:.2}ms vs {:.2}ms)", ratio, fallback_p.p50_ms(), find_p.p50_ms()),
    );

    let fallback_passed = true; // informational test
    reporter::print_result(
        "2.7 FIND non-anchor fallback",
        fallback_passed,
        &format!("p50={:.2}ms, {:.1}x slower", fallback_p.p50_ms(), ratio),
        &format!("{} LIDs found", fallback_total_lids),
    );

    results.push(TestResult {
        name: "2.7 FIND non-anchor fallback".into(),
        passed: fallback_passed,
        value: format!("p50={:.2}ms p99={:.2}ms", fallback_p.p50_ms(), fallback_p.p99_ms()),
        expected: "informational (slower than anchor)".into(),
        notes: format!("{:.1}x slower than anchor, {} LIDs", ratio, fallback_total_lids),
    });

    // ── Summary ─────────────────────────────────────────────────────────

    let suite_elapsed = suite_start.elapsed();
    reporter::print_separator();
    reporter::print_metric(
        "Suite 2 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 2: Read Patterns".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}
