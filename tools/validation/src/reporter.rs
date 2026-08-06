// SPDX-License-Identifier: BUSL-1.1
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default, Serialize)]
pub struct Report {
    pub config: BTreeMap<String, String>,
    pub suites: BTreeMap<String, SuiteReport>,
}

#[derive(Default, Serialize)]
pub struct SuiteReport {
    pub name: String,
    pub passed: bool,
    pub duration_secs: f64,
    pub results: Vec<TestResult>,
    pub errors: Vec<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub value: String,
    pub expected: String,
    pub notes: String,
}

impl Report {
    pub fn save_json(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

pub fn print_suite_header(name: &str) {
    println!("\n══════════════════════════════════════════════════════════════════════");
    println!("  {name}");
    println!("══════════════════════════════════════════════════════════════════════\n");
}

pub fn print_result(name: &str, passed: bool, value: &str, notes: &str) {
    let status = if passed { "PASS" } else { "FAIL" };
    let marker = if passed { "✓" } else { "✗" };
    println!("  [{status}] {marker} {name:40} {value:>15}  {notes}");
}

pub fn print_metric(label: &str, value: &str) {
    println!("  {:42} {}", label, value);
}

pub fn print_separator() {
    println!("  ──────────────────────────────────────────────────────────────────");
}

pub fn format_num(n: u64) -> String {
    let s = n.to_string();
    let mut r = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { r.push(','); }
        r.push(c);
    }
    r.chars().rev().collect()
}
