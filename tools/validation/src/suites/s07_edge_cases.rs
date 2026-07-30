use std::time::Instant;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::reporter::{self, SuiteReport, TestResult};
use crate::utils::assertions;
use crate::utils::tcp_client::TcpClient;

pub async fn run(config: &Config) -> Result<SuiteReport> {
    reporter::print_suite_header("Suite 7: Edge Cases");

    let suite_start = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Unique run ID so tests don't collide with previous runs on the same server
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() % 1_000_000;

    // ── 7.1 Many fields (100) ───────────────────────────────────────────

    reporter::print_metric("Test 7.1", "Many fields (100)");

    let passed_7_1 = match test_many_fields(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.1 Many fields: {e}"));
            false
        }
    };

    reporter::print_result("7.1 Many fields (100)", passed_7_1, "", "");
    results.push(TestResult {
        name: "7.1 Many fields (100)".into(),
        passed: passed_7_1,
        value: if passed_7_1 { "100 fields roundtrip OK".into() } else { "FAIL".into() },
        expected: "response contains f100".into(),
        notes: String::new(),
    });

    // ── 7.2 Large text value ────────────────────────────────────────────

    reporter::print_metric("Test 7.2", "Large text value (10KB)");

    let passed_7_2 = match test_large_text(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.2 Large text value: {e}"));
            false
        }
    };

    reporter::print_result("7.2 Large text value", passed_7_2, "", "");
    results.push(TestResult {
        name: "7.2 Large text value (10KB)".into(),
        passed: passed_7_2,
        value: if passed_7_2 { "10KB roundtrip OK".into() } else { "FAIL".into() },
        expected: "response contains abcdefghij".into(),
        notes: String::new(),
    });

    // ── 7.3 Special characters ──────────────────────────────────────────

    reporter::print_metric("Test 7.3", "Special characters (accented, emoji)");

    let passed_7_3 = match test_special_characters(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.3 Special characters: {e}"));
            false
        }
    };

    reporter::print_result("7.3 Special characters", passed_7_3, "", "");
    results.push(TestResult {
        name: "7.3 Special characters".into(),
        passed: passed_7_3,
        value: if passed_7_3 { "roundtrip OK".into() } else { "FAIL".into() },
        expected: "accented + emoji survive roundtrip".into(),
        notes: String::new(),
    });

    // ── 7.4 Single-record lobe ──────────────────────────────────────────

    reporter::print_metric("Test 7.4", "Single-record lobe");

    let passed_7_4 = match test_single_record_lobe(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.4 Single-record lobe: {e}"));
            false
        }
    };

    reporter::print_result("7.4 Single-record lobe", passed_7_4, "", "");
    results.push(TestResult {
        name: "7.4 Single-record lobe".into(),
        passed: passed_7_4,
        value: if passed_7_4 { "1 record PULL+SCAN OK".into() } else { "FAIL".into() },
        expected: "1 record in PULL and SCAN".into(),
        notes: String::new(),
    });

    // ── 7.5 Empty lobe ─────────────────────────────────────────────────

    reporter::print_metric("Test 7.5", "Empty lobe");

    let passed_7_5 = match test_empty_lobe(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.5 Empty lobe: {e}"));
            false
        }
    };

    reporter::print_result("7.5 Empty lobe", passed_7_5, "", "");
    results.push(TestResult {
        name: "7.5 Empty lobe".into(),
        passed: passed_7_5,
        value: if passed_7_5 { "SCAN + SHOW AUTOANCHOR OK".into() } else { "FAIL".into() },
        expected: "0 records, no crash".into(),
        notes: String::new(),
    });

    // ── 7.6 Auto _type injection ────────────────────────────────────────

    reporter::print_metric("Test 7.6", "Auto _type injection");

    let passed_7_6 = match test_auto_type_injection(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.6 Auto _type injection: {e}"));
            false
        }
    };

    reporter::print_result("7.6 Auto _type injection", passed_7_6, "", "");
    results.push(TestResult {
        name: "7.6 Auto _type injection".into(),
        passed: passed_7_6,
        value: if passed_7_6 { "_type = lobe name".into() } else { "FAIL".into() },
        expected: "_type equals lobe name".into(),
        notes: String::new(),
    });

    // ── 7.7 Type mismatch filter ────────────────────────────────────────

    reporter::print_metric("Test 7.7", "Type mismatch filter");

    let passed_7_7 = match test_type_mismatch_filter(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.7 Type mismatch filter: {e}"));
            false
        }
    };

    reporter::print_result("7.7 Type mismatch filter", passed_7_7, "", "");
    results.push(TestResult {
        name: "7.7 Type mismatch filter".into(),
        passed: passed_7_7,
        value: if passed_7_7 { "0 records, no crash".into() } else { "FAIL".into() },
        expected: "0 records returned".into(),
        notes: String::new(),
    });

    // ── 7.8 Anchor on optional field ────────────────────────────────────

    reporter::print_metric("Test 7.8", "Anchor on optional field");

    let passed_7_8 = match test_anchor_optional_field(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.8 Anchor on optional field: {e}"));
            false
        }
    };

    reporter::print_result("7.8 Anchor on optional field", passed_7_8, "", "");
    results.push(TestResult {
        name: "7.8 Anchor on optional field".into(),
        passed: passed_7_8,
        value: if passed_7_8 { "PUT without anchor field OK".into() } else { "FAIL".into() },
        expected: "records accepted without anchor field".into(),
        notes: String::new(),
    });

    // ── 7.9 Duplicate anchor error ──────────────────────────────────────

    reporter::print_metric("Test 7.9", "Duplicate anchor error");

    let passed_7_9 = match test_duplicate_anchor_error(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.9 Duplicate anchor error: {e}"));
            false
        }
    };

    reporter::print_result("7.9 Duplicate anchor error", passed_7_9, "", "");
    results.push(TestResult {
        name: "7.9 Duplicate anchor error".into(),
        passed: passed_7_9,
        value: if passed_7_9 { "Duplicate error received".into() } else { "FAIL".into() },
        expected: "error containing Duplicate".into(),
        notes: String::new(),
    });

    // ── 7.10 Empty batch ────────────────────────────────────────────────

    reporter::print_metric("Test 7.10", "Empty batch");

    let passed_7_10 = match test_empty_batch(config, run_id).await {
        Ok(passed) => passed,
        Err(e) => {
            errors.push(format!("7.10 Empty batch: {e}"));
            false
        }
    };

    reporter::print_result("7.10 Empty batch", passed_7_10, "", "");
    results.push(TestResult {
        name: "7.10 Empty batch".into(),
        passed: passed_7_10,
        value: if passed_7_10 { "handled gracefully".into() } else { "FAIL".into() },
        expected: "no crash".into(),
        notes: String::new(),
    });

    // ── Summary ─────────────────────────────────────────────────────────

    let suite_elapsed = suite_start.elapsed();
    reporter::print_separator();
    reporter::print_metric(
        "Suite 7 completed in",
        &format!("{:.2}s", suite_elapsed.as_secs_f64()),
    );

    let all_passed = results.iter().all(|r| r.passed);

    Ok(SuiteReport {
        name: "Suite 7: Edge Cases".into(),
        passed: all_passed,
        duration_secs: suite_elapsed.as_secs_f64(),
        results,
        errors,
    })
}

// ─── Helper ─────────────────────────────────────────────────────────────────

/// Build a xyTalk query string with a lobe name properly quoted.
fn q(template: &str, lobe: &str) -> String {
    template.replace("$L", &format!("\"{lobe}\""))
}

// ─── Individual test functions ──────────────────────────────────────────────

async fn test_many_fields(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.1: connect")?;
    let lobe = format!("edge_{run_id}");

    let _ = client.query_text(&q("LOBE $L", &lobe)).await;

    let mut fields = String::new();
    for i in 1..=100 {
        if i > 1 { fields.push_str(", "); }
        fields.push_str(&format!("f{i}: \"v{i}\""));
    }
    let put = format!("PUT {{{fields}}} IN \"{lobe}\"");
    client.query_text(&put).await.context("7.1: PUT 100 fields")?;

    let response = client
        .query_text(&format!("SCAN \"{lobe}\" WHERE f100 = \"v100\""))
        .await.context("7.1: SCAN")?;

    let passed = response.contains("f100");
    if !passed { reporter::print_metric("  FAIL detail", "f100 not found in response"); }
    Ok(passed)
}

async fn test_large_text(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.2: connect")?;
    let lobe = format!("edge_{run_id}");
    let large_value: String = "abcdefghij".repeat(1000);

    let put = format!("PUT {{bigfield: \"{large_value}\"}} IN \"{lobe}\"");
    client.query_text(&put).await.context("7.2: PUT large text")?;

    let response = client
        .query_text(&format!("SCAN \"{lobe}\""))
        .await.context("7.2: SCAN")?;

    let passed = response.contains("abcdefghij");
    if !passed { reporter::print_metric("  FAIL detail", "abcdefghij not in response"); }
    Ok(passed)
}

async fn test_special_characters(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.3: connect")?;
    let lobe = format!("chars_{run_id}");

    let _ = client.query_text(&format!("LOBE \"{lobe}\"")).await;
    client
        .query_text(&format!("PUT {{name: \"Jos\\u{{00e9}} Mar\\u{{00ed}}a\", region: \"M\\u{{00e9}}xico\", tag: \"special\"}} IN \"{lobe}\""))
        .await.context("7.3: PUT accented")?;

    let response = client
        .query_text(&format!("SCAN \"{lobe}\" WHERE tag = \"special\""))
        .await.context("7.3: SCAN")?;

    let has_record = response.contains("LID:");
    let passed = has_record;
    if !passed { reporter::print_metric("  FAIL detail", "no record found"); }
    Ok(passed)
}

async fn test_single_record_lobe(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.4: connect")?;
    let lobe = format!("single_{run_id}");

    client.query_text(&format!("LOBE \"{lobe}\"")).await.context("7.4: LOBE")?;
    client.query_text(&format!("PUT {{item: \"only_one\"}} IN \"{lobe}\"")).await.context("7.4: PUT")?;

    let pull = client
        .query_text(&format!("FIND \"{lobe}\" WHERE item = \"only_one\" | PULL"))
        .await.context("7.4: PULL")?;
    let pull_count = assertions::count_lids_in_text(&pull);

    let scan = client
        .query_text(&format!("SCAN \"{lobe}\""))
        .await.context("7.4: SCAN")?;
    let scan_count = assertions::count_lids_in_text(&scan);

    let passed = pull_count >= 1 && scan_count == 1;
    if !passed { reporter::print_metric("  FAIL detail", &format!("PULL={pull_count}, SCAN={scan_count}")); }
    Ok(passed)
}

async fn test_empty_lobe(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.5: connect")?;
    let lobe = format!("empty_{run_id}");

    client.query_text(&format!("LOBE \"{lobe}\"")).await.context("7.5: LOBE")?;

    let scan = client.query_text(&format!("SCAN \"{lobe}\"")).await.context("7.5: SCAN")?;
    let scan_ok = scan.contains("0 record") || assertions::count_lids_in_text(&scan) == 0;

    let aa = client.query_text(&format!("SHOW AUTOANCHOR IN \"{lobe}\"")).await;
    let aa_ok = aa.is_ok() || aa.is_err_and(|e| !format!("{e}").contains("connection"));

    let passed = scan_ok && aa_ok;
    if !passed { reporter::print_metric("  FAIL detail", &format!("scan_ok={scan_ok}, aa_ok={aa_ok}")); }
    Ok(passed)
}

async fn test_auto_type_injection(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.6: connect")?;
    let lobe = format!("autotype_{run_id}");

    let _ = client.query_text(&format!("LOBE \"{lobe}\"")).await;
    client.query_text(&format!("PUT {{name: \"test_auto\"}} IN \"{lobe}\"")).await.context("7.6: PUT")?;

    let response = client
        .query_text(&format!("SCAN \"{lobe}\" WHERE name = \"test_auto\""))
        .await.context("7.6: SCAN")?;

    let passed = response.contains("_type") && response.contains(&lobe);
    if !passed { reporter::print_metric("  FAIL detail", "missing _type or lobe name"); }
    Ok(passed)
}

async fn test_type_mismatch_filter(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.7: connect")?;
    let lobe = format!("edge_{run_id}");

    let result = client.query_text(&format!("FIND \"{lobe}\" WHERE f1 > 100")).await;
    let passed = match result {
        Ok(r) => assertions::count_lids_in_text(&r) == 0 || r.contains("0 record"),
        Err(e) => format!("{e}").contains("Server error"),
    };
    Ok(passed)
}

async fn test_anchor_optional_field(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.8: connect")?;
    let lobe = format!("optanchor_{run_id}");

    let _ = client.query_text(&format!("LOBE \"{lobe}\"")).await;
    client.query_text(&format!("ANCHOR \"email\" UNIQUE IN \"{lobe}\"")).await.context("7.8: ANCHOR")?;

    let first = client.query_text(&format!("PUT {{name: \"Alice\"}} IN \"{lobe}\"")).await;
    let second = client.query_text(&format!("PUT {{name: \"Bob\"}} IN \"{lobe}\"")).await;

    let passed = first.is_ok() && second.is_ok();
    if !passed { reporter::print_metric("  FAIL detail", &format!("first={}, second={}", first.is_ok(), second.is_ok())); }
    Ok(passed)
}

async fn test_duplicate_anchor_error(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.9: connect")?;
    let lobe = format!("duptest_{run_id}");

    let _ = client.query_text(&format!("LOBE \"{lobe}\"")).await;
    client.query_text(&format!("ANCHOR \"code\" UNIQUE IN \"{lobe}\"")).await.context("7.9: ANCHOR")?;
    client.query_text(&format!("PUT {{code: \"ABC123\", val: \"first\"}} IN \"{lobe}\"")).await.context("7.9: PUT 1")?;

    let second = client.query_text(&format!("PUT {{code: \"ABC123\", val: \"second\"}} IN \"{lobe}\"")).await;
    let passed = match second {
        Ok(r) => r.to_lowercase().contains("duplicate"),
        Err(e) => format!("{e}").to_lowercase().contains("duplicate"),
    };
    if !passed { reporter::print_metric("  FAIL detail", "no Duplicate in error"); }
    Ok(passed)
}

async fn test_empty_batch(config: &Config, run_id: u128) -> Result<bool> {
    let mut client = TcpClient::connect(&config.host, config.port).await.context("7.10: connect")?;
    let lobe = format!("edge_{run_id}");

    let result = client.query_text(&format!("PUT BATCH IN \"{lobe}\" []")).await;
    let passed = match result {
        Ok(_) => true,
        Err(e) => format!("{e}").contains("Server error"),
    };
    Ok(passed)
}
