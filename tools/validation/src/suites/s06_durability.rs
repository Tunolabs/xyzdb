// SPDX-License-Identifier: BUSL-1.1
use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use tokio::process::Command;

use crate::config::Config;
use crate::data_generator::DomainGenerator;
use crate::reporter::{self, format_num, SuiteReport, TestResult};
use crate::utils::assertions;
use crate::utils::tcp_client::TcpClient;

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

/// Spawn the xyzdb-server process.
async fn spawn_server(config: &Config, port: u16) -> Result<tokio::process::Child> {
    let child = Command::new(&config.server_bin)
        .arg("--path")
        .arg(&config.db_path)
        .arg("--port")
        .arg(port.to_string())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "spawn server binary {:?} --path {:?} --port {}",
                config.server_bin, config.db_path, port
            )
        })?;
    Ok(child)
}

/// Clean up the database directory.
fn cleanup_db_path(config: &Config) {
    let _ = std::fs::remove_dir_all(&config.db_path);
}

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 6: Durability and Recovery");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let port = config.port + 100;

    // ── Clean up any leftover DB from a previous run ─────────────────────
    cleanup_db_path(config);

    // ── Phase 6.1: Start server ──────────────────────────────────────────

    reporter::print_metric("Phase 6.1", "Start server (own instance)");

    let mut server = spawn_server(config, port)
        .await
        .context("Phase 6.1: spawn server")?;

    let mut client = match wait_for_server(&config.host, port).await {
        Ok(c) => c,
        Err(e) => {
            let _ = server.kill().await;
            let _ = server.wait().await;
            cleanup_db_path(config);
            return Err(e).context("Phase 6.1: wait for server startup");
        }
    };

    reporter::print_metric("  Server started on port", &port.to_string());

    results.push(TestResult {
        name: "Phase 6.1: Start server".into(),
        passed: true,
        value: format!("port {port}"),
        expected: "server running".into(),
        notes: format!("binary: {:?}", config.server_bin),
    });

    // ── Phase 6.2: Setup (LOBE + ANCHORs) ───────────────────────────────

    reporter::print_metric("Phase 6.2", "Setup LOBE + ANCHORs");

    client
        .exec(r#"LOBE "catalog""#)
        .await
        .context("6.2: create LOBE catalog")?;

    client
        .exec(r#"ANCHOR "code" UNIQUE IN "catalog""#)
        .await
        .context("6.2: create ANCHOR on code")?;

    client
        .exec(r#"ANCHOR "project_id" UNIQUE IN "catalog""#)
        .await
        .context("6.2: create ANCHOR on project_id")?;

    results.push(TestResult {
        name: "Phase 6.2: Setup LOBE + ANCHORs".into(),
        passed: true,
        value: "OK".into(),
        expected: "OK".into(),
        notes: "LOBE catalog + 2 ANCHORs".into(),
    });

    // ── Phase 6.3: Load 1,000 companies with hierarchies ─────────────────

    reporter::print_metric(
        "Phase 6.3",
        &format!("Load {} companies with hierarchies", format_num(1_000)),
    );

    let mut datagen = DomainGenerator::new(42);
    let mut all_codes: Vec<String> = Vec::with_capacity(1_000);

    for i in 0..1_000u32 {
        let hierarchy = datagen.generate_company_hierarchy(i);

        client
            .exec(&hierarchy.company_query)
            .await
            .with_context(|| format!("PUT company {i}"))?;

        all_codes.push(hierarchy.company_code.clone());

        for project in &hierarchy.projects {
            client
                .exec(&project.project_query)
                .await
                .with_context(|| format!("PUT project {}", project.project_id))?;

            client
                .exec(&project.task_batch)
                .await
                .with_context(|| format!("PUT BATCH tasks for {}", project.project_id))?;
        }
    }

    reporter::print_metric("  Loaded", &format!("{} companies + hierarchies", format_num(1_000)));

    results.push(TestResult {
        name: "Phase 6.3: Load data".into(),
        passed: true,
        value: format!("{} companies", format_num(1_000)),
        expected: format!("{} companies", format_num(1_000)),
        notes: "with projects + tasks".into(),
    });

    reporter::print_separator();

    // ── Phase 6.4: Verify pre-restart ────────────────────────────────────

    reporter::print_metric("Phase 6.4", "Verify pre-restart state");

    // 6.4a: SCAN + AGGREGATE counts by _type
    let pre_company_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Company" AGGREGATE count()"#)
        .await
        .context("6.4: AGGREGATE count() Company")?;
    let pre_company_count =
        assertions::get_aggregate_int(&pre_company_result, "count").unwrap_or(0);

    let pre_project_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Project" AGGREGATE count()"#)
        .await
        .context("6.4: AGGREGATE count() Project")?;
    let pre_project_count =
        assertions::get_aggregate_int(&pre_project_result, "count").unwrap_or(0);

    let pre_task_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Task" AGGREGATE count()"#)
        .await
        .context("6.4: AGGREGATE count() Task")?;
    let pre_task_count =
        assertions::get_aggregate_int(&pre_task_result, "count").unwrap_or(0);

    // Store expected counts for post-restart comparison
    let expected_counts: HashMap<&str, i64> = HashMap::from([
        ("Company", pre_company_count),
        ("Project", pre_project_count),
        ("Task", pre_task_count),
    ]);

    let counts_ok = pre_company_count == 1_000;
    reporter::print_metric(
        "  Pre-restart counts",
        &format!(
            "Company={}, Project={}, Task={}",
            format_num(pre_company_count as u64),
            format_num(pre_project_count as u64),
            format_num(pre_task_count as u64),
        ),
    );

    if !counts_ok {
        errors.push(format!(
            "Pre-restart company count mismatch: expected 1,000, got {}",
            pre_company_count
        ));
    }

    results.push(TestResult {
        name: "Phase 6.4a: Pre-restart counts".into(),
        passed: counts_ok,
        value: format!(
            "Co={} Pr={} T={}",
            format_num(pre_company_count as u64),
            format_num(pre_project_count as u64),
            format_num(pre_task_count as u64),
        ),
        expected: format!("Company={}", format_num(1_000)),
        notes: if counts_ok { "exact match".into() } else { "MISMATCH".into() },
    });

    // 6.4b: 50 random FINDs by code
    let mut rng = StdRng::seed_from_u64(99);
    let mut shuffled_find = all_codes.clone();
    shuffled_find.shuffle(&mut rng);
    let sample_find_codes: Vec<&String> = shuffled_find.iter().take(50).collect();
    let mut pre_find_results: Vec<(String, usize)> = Vec::with_capacity(50);
    let mut find_pre_ok = true;

    for code in &sample_find_codes {
        let q = format!(r#"FIND "catalog" WHERE code = "{}""#, code);
        let result = client.query_bin(&q).await.context("6.4b: FIND by code")?;
        let count = assertions::record_count(&result);
        pre_find_results.push((code.to_string(), count));
        if count != 1 {
            find_pre_ok = false;
            errors.push(format!("Pre-restart FIND code={} returned {} records, expected 1", code, count));
        }
    }

    reporter::print_metric(
        "  Pre-restart FIND by code",
        &format!("50 lookups, all=1 record: {}", if find_pre_ok { "YES" } else { "NO" }),
    );

    results.push(TestResult {
        name: "Phase 6.4b: Pre-restart FINDs".into(),
        passed: find_pre_ok,
        value: format!("50/50 returned 1 record: {}", find_pre_ok),
        expected: "each returns 1 record".into(),
        notes: String::new(),
    });

    // 6.4c: 10 random PULLs
    let mut shuffled_pull = all_codes.clone();
    shuffled_pull.shuffle(&mut rng);
    let sample_pull_codes: Vec<&String> = shuffled_pull.iter().take(10).collect();
    let mut pre_pull_results: Vec<(String, usize)> = Vec::with_capacity(10);
    let mut pull_pre_ok = true;

    for code in &sample_pull_codes {
        let q = format!(r#"FIND "catalog" WHERE code = "{}" | PULL depth=1"#, code);
        let result = client.query_bin(&q).await.context("6.4c: PULL")?;
        let count = assertions::record_count(&result);
        pre_pull_results.push((code.to_string(), count));
        if count <= 1 {
            pull_pre_ok = false;
            errors.push(format!("Pre-restart PULL code={} returned {} records, expected >1", code, count));
        }
    }

    reporter::print_metric(
        "  Pre-restart PULL",
        &format!("10 lookups, all >1 record: {}", if pull_pre_ok { "YES" } else { "NO" }),
    );

    results.push(TestResult {
        name: "Phase 6.4c: Pre-restart PULLs".into(),
        passed: pull_pre_ok,
        value: format!("10 PULLs, all >1: {}", pull_pre_ok),
        expected: "each returns >1 record".into(),
        notes: String::new(),
    });

    reporter::print_separator();

    // ── Phase 6.5: Kill server ───────────────────────────────────────────

    reporter::print_metric("Phase 6.5", "Kill server (SIGTERM)");

    // Drop the client connection before killing
    drop(client);

    server.kill().await.context("6.5: kill server")?;
    server.wait().await.context("6.5: wait for server exit")?;

    reporter::print_metric("  Server stopped", "OK");

    results.push(TestResult {
        name: "Phase 6.5: Kill server".into(),
        passed: true,
        value: "stopped".into(),
        expected: "clean shutdown".into(),
        notes: "SIGTERM sent, process exited".into(),
    });

    reporter::print_separator();

    // ── Phase 6.6: Restart server ────────────────────────────────────────

    reporter::print_metric("Phase 6.6", "Restart server (same path + port)");

    let mut server = spawn_server(config, port)
        .await
        .context("Phase 6.6: re-spawn server")?;

    let mut client = match wait_for_server(&config.host, port).await {
        Ok(c) => c,
        Err(e) => {
            let _ = server.kill().await;
            let _ = server.wait().await;
            cleanup_db_path(config);
            return Err(e).context("Phase 6.6: wait for server restart");
        }
    };

    reporter::print_metric("  Server restarted on port", &port.to_string());

    results.push(TestResult {
        name: "Phase 6.6: Restart server".into(),
        passed: true,
        value: format!("port {port}"),
        expected: "server running".into(),
        notes: "same db_path, same port".into(),
    });

    reporter::print_separator();

    // ── Phase 6.7: Verify post-restart ───────────────────────────────────

    reporter::print_metric("Phase 6.7", "Verify post-restart state");

    // 6.7a: Same 50 FINDs must return same records
    let mut find_post_ok = true;
    let mut find_mismatches = 0u32;

    for (code, pre_count) in &pre_find_results {
        let q = format!(r#"FIND "catalog" WHERE code = "{}""#, code);
        let result = client
            .query_bin(&q)
            .await
            .context("6.7a: post-restart FIND by code")?;
        let post_count = assertions::record_count(&result);
        if post_count != *pre_count {
            find_post_ok = false;
            find_mismatches += 1;
            errors.push(format!(
                "Post-restart FIND code={}: pre={}, post={}",
                code, pre_count, post_count
            ));
        }
    }

    reporter::print_metric(
        "  Post-restart FINDs",
        &format!(
            "50 lookups, matches pre-restart: {}{}",
            if find_post_ok { "YES" } else { "NO" },
            if find_mismatches > 0 {
                format!(" ({} mismatches)", find_mismatches)
            } else {
                String::new()
            }
        ),
    );

    results.push(TestResult {
        name: "Phase 6.7a: Post-restart FINDs".into(),
        passed: find_post_ok,
        value: format!("{}/50 match", 50 - find_mismatches),
        expected: "50/50 match pre-restart".into(),
        notes: if find_post_ok {
            "all records persisted".into()
        } else {
            format!("{} mismatches", find_mismatches)
        },
    });

    // 6.7b: Same 10 PULLs must return same record counts
    let mut pull_post_ok = true;
    let mut pull_mismatches = 0u32;

    for (code, pre_count) in &pre_pull_results {
        let q = format!(r#"FIND "catalog" WHERE code = "{}" | PULL depth=1"#, code);
        let result = client
            .query_bin(&q)
            .await
            .context("6.7b: post-restart PULL")?;
        let post_count = assertions::record_count(&result);
        if post_count != *pre_count {
            pull_post_ok = false;
            pull_mismatches += 1;
            errors.push(format!(
                "Post-restart PULL code={}: pre={}, post={}",
                code, pre_count, post_count
            ));
        }
    }

    reporter::print_metric(
        "  Post-restart PULLs",
        &format!(
            "10 lookups, matches pre-restart: {}{}",
            if pull_post_ok { "YES" } else { "NO" },
            if pull_mismatches > 0 {
                format!(" ({} mismatches)", pull_mismatches)
            } else {
                String::new()
            }
        ),
    );

    results.push(TestResult {
        name: "Phase 6.7b: Post-restart PULLs".into(),
        passed: pull_post_ok,
        value: format!("{}/10 match", 10 - pull_mismatches),
        expected: "10/10 match pre-restart".into(),
        notes: if pull_post_ok {
            "all hierarchies persisted".into()
        } else {
            format!("{} mismatches", pull_mismatches)
        },
    });

    // 6.7c: SCAN + AGGREGATE counts must match pre-restart
    let post_company_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Company" AGGREGATE count()"#)
        .await
        .context("6.7c: post-restart AGGREGATE Company")?;
    let post_company_count =
        assertions::get_aggregate_int(&post_company_result, "count").unwrap_or(0);

    let post_project_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Project" AGGREGATE count()"#)
        .await
        .context("6.7c: post-restart AGGREGATE Project")?;
    let post_project_count =
        assertions::get_aggregate_int(&post_project_result, "count").unwrap_or(0);

    let post_task_result = client
        .query_bin(r#"SCAN "catalog" WHERE _type = "Task" AGGREGATE count()"#)
        .await
        .context("6.7c: post-restart AGGREGATE Task")?;
    let post_task_count =
        assertions::get_aggregate_int(&post_task_result, "count").unwrap_or(0);

    let company_match = post_company_count == *expected_counts.get("Company").unwrap_or(&0);
    let project_match = post_project_count == *expected_counts.get("Project").unwrap_or(&0);
    let task_match =
        post_task_count == *expected_counts.get("Task").unwrap_or(&0);
    let counts_post_ok = company_match && project_match && task_match;

    reporter::print_metric(
        "  Post-restart counts",
        &format!(
            "Company={} ({}), Project={} ({}), Task={} ({})",
            format_num(post_company_count as u64),
            if company_match { "OK" } else { "MISMATCH" },
            format_num(post_project_count as u64),
            if project_match { "OK" } else { "MISMATCH" },
            format_num(post_task_count as u64),
            if task_match { "OK" } else { "MISMATCH" },
        ),
    );

    if !company_match {
        errors.push(format!(
            "Post-restart Company count: pre={}, post={}",
            expected_counts.get("Company").unwrap_or(&0),
            post_company_count
        ));
    }
    if !project_match {
        errors.push(format!(
            "Post-restart Project count: pre={}, post={}",
            expected_counts.get("Project").unwrap_or(&0),
            post_project_count
        ));
    }
    if !task_match {
        errors.push(format!(
            "Post-restart Task count: pre={}, post={}",
            expected_counts.get("Task").unwrap_or(&0),
            post_task_count
        ));
    }

    results.push(TestResult {
        name: "Phase 6.7c: Post-restart counts".into(),
        passed: counts_post_ok,
        value: format!(
            "Co={} Pr={} T={}",
            format_num(post_company_count as u64),
            format_num(post_project_count as u64),
            format_num(post_task_count as u64),
        ),
        expected: format!(
            "Co={} Pr={} T={}",
            format_num(pre_company_count as u64),
            format_num(pre_project_count as u64),
            format_num(pre_task_count as u64),
        ),
        notes: if counts_post_ok {
            "all counts match".into()
        } else {
            "MISMATCH".into()
        },
    });

    // 6.7d: Duplicate anchor must fail (anchors persisted)
    let first_code = &all_codes[0];
    let dup_query = format!(
        r#"PUT {{_type: "Company", code: "{}", name: "Duplicate", description: "Test"}} IN "catalog""#,
        first_code
    );
    let dup_result = client.query_text(&dup_query).await;

    let dup_rejected = match dup_result {
        Ok(response) => {
            let has_dup = response.to_lowercase().contains("duplicate");
            if !has_dup {
                reporter::print_metric("  WARNING", "duplicate PUT succeeded without error");
            }
            has_dup
        }
        Err(e) => {
            let msg = format!("{e}");
            let has_dup = msg.to_lowercase().contains("duplicate");
            if !has_dup {
                reporter::print_metric(
                    "  WARNING",
                    &format!("error but no Duplicate keyword: {msg}"),
                );
            }
            has_dup
        }
    };

    reporter::print_metric(
        "  Duplicate anchor rejection",
        &format!(
            "code={}: {}",
            first_code,
            if dup_rejected { "correctly rejected" } else { "NOT rejected" }
        ),
    );

    if !dup_rejected {
        errors.push(format!(
            "Post-restart duplicate anchor code={} was not rejected",
            first_code
        ));
    }

    results.push(TestResult {
        name: "Phase 6.7d: Duplicate anchor post-restart".into(),
        passed: dup_rejected,
        value: if dup_rejected {
            "rejected".into()
        } else {
            "NOT rejected".into()
        },
        expected: "Duplicate error".into(),
        notes: format!("code={}", first_code),
    });

    reporter::print_separator();

    // ── Phase 6.8: Cleanup ───────────────────────────────────────────────

    reporter::print_metric("Phase 6.8", "Cleanup");

    drop(client);

    let _ = server.kill().await;
    let _ = server.wait().await;

    cleanup_db_path(config);

    reporter::print_metric("  Server stopped", "OK");
    reporter::print_metric("  DB path removed", &format!("{:?}", config.db_path));

    results.push(TestResult {
        name: "Phase 6.8: Cleanup".into(),
        passed: true,
        value: "OK".into(),
        expected: "clean".into(),
        notes: "server killed, db_path removed".into(),
    });

    // ── Summary ──────────────────────────────────────────────────────────

    let suite_elapsed = suite_start.elapsed();
    reporter::print_separator();
    reporter::print_metric(
        "Suite 6 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 6: Durability and Recovery".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}
