// SPDX-License-Identifier: BUSL-1.1
mod config;
mod data_generator;
mod monitor;
mod reporter;
mod suites;
mod utils;

use clap::Parser;
use config::Config;
use reporter::{Report, format_num};
use std::collections::BTreeMap;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    let est_records = data_generator::DomainGenerator::estimate_records(config.clients);

    println!("\n╔══════════════════════════════════════════════════════════════════════╗");
    println!("║  xyzDB Validation Suite                                            ║");
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!("║  Server:  {:58}║", config.addr());
    println!("║  Clients: {:58}║", format_num(config.clients as u64));
    println!("║  Est. records: {:53}║", format_num(est_records));
    println!("║  Suite:   {:58}║", config.suite);
    println!("╚══════════════════════════════════════════════════════════════════════╝\n");

    let mut report = Report::default();
    report.config.insert("host".into(), config.host.clone());
    report.config.insert("port".into(), config.port.to_string());
    report.config.insert("clients".into(), config.clients.to_string());
    report.config.insert("suite".into(), config.suite.clone());

    let global_start = Instant::now();
    let mut all_passed = true;

    // Suite 1: Data Load
    if config.should_run(1) {
        let result = suites::s01_data_load::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s01_data_load".into(), result);
    }

    // Suite 2: Read Patterns (requires data from Suite 1)
    if config.should_run(2) {
        let result = suites::s02_read_patterns::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s02_read_patterns".into(), result);
    }

    // Suite 3: Write Stress
    if config.should_run(3) {
        let result = suites::s03_write_stress::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s03_write_stress".into(), result);
    }

    // Suite 4: Mixed Workload
    if config.should_run(4) {
        let result = suites::s04_mixed_workload::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s04_mixed_workload".into(), result);
    }

    // Suite 5: Connection Management
    if config.should_run(5) {
        let result = suites::s05_connections::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s05_connections".into(), result);
    }

    // Suite 7: Edge Cases
    if config.should_run(7) {
        let result = suites::s07_edge_cases::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s07_edge_cases".into(), result);
    }

    // Suite 8: Auto-Discovery
    if config.should_run(8) {
        let result = suites::s08_autodiscovery::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s08_autodiscovery".into(), result);
    }

    // Suite 6: Durability (spawns own server)
    if config.should_run(6) {
        let result = suites::s06_durability::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s06_durability".into(), result);
    }

    // Suite 9: Scale Curve (spawns own server)
    if config.should_run(9) {
        let result = suites::s09_scale_curve::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s09_scale_curve".into(), result);
    }

    // Suite 10: Endurance (10 min sustained load)
    if config.should_run(10) {
        let result = suites::s10_endurance::run(&config).await?;
        if !result.passed { all_passed = false; }
        report.suites.insert("s10_endurance".into(), result);
    }

    let total_time = global_start.elapsed();

    // Summary
    println!("\n╔══════════════════════════════════════════════════════════════════════╗");
    println!("║  VALIDATION SUMMARY                                                ║");
    println!("╠══════════════════════════════════════════════════════════════════════╣");

    for (name, suite) in &report.suites {
        let status = if suite.passed { "PASS" } else { "FAIL" };
        let marker = if suite.passed { "✓" } else { "✗" };
        let tests = suite.results.len();
        let passed_count = suite.results.iter().filter(|t| t.passed).count();
        println!(
            "║  [{status}] {marker} {:30} {:3}/{:3} tests  {:8.1}s       ║",
            name, passed_count, tests, suite.duration_secs
        );
    }

    let overall = if all_passed { "ALL PASSED" } else { "SOME FAILED" };
    println!("╠══════════════════════════════════════════════════════════════════════╣");
    println!(
        "║  {overall:20}                   Total: {:.1}s              ║",
        total_time.as_secs_f64()
    );
    println!("╚══════════════════════════════════════════════════════════════════════╝\n");

    // Save JSON report if requested
    if let Some(ref path) = config.report {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        report.save_json(path)?;
        println!("  Report saved to: {}\n", path.display());
    }

    if !all_passed {
        std::process::exit(1);
    }

    Ok(())
}
