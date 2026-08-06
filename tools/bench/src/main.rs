// SPDX-License-Identifier: BUSL-1.1
use anyhow::{Context, Result, bail};
use clap::Parser;
use rand::prelude::IndexedRandom;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Parser)]
#[command(name = "xyzdb-bench", about = "xyzDB MVP Benchmark — Full Stack")]
struct Args {
    #[arg(long, default_value = "localhost")]
    host: String,
    #[arg(long, default_value_t = 2505)]
    port: u16,
    /// Which test to run: setup, write, find, pull, scan, mixed, mutations, all
    #[arg(long, default_value = "all")]
    test: String,
    /// Number of companies to generate
    #[arg(long, default_value_t = 100_000)]
    clients: u32,
    /// Label for this run
    #[arg(long, default_value = "unlabeled")]
    label: String,
}

const STATUS_OK: u8 = 0x00;
const WARMUP: usize = 100;

// Protocol versions
const V1: u8 = 1;
const V2: u8 = 2;
#[allow(dead_code)]
const FORMAT_TEXT: u8 = 0x00;
const FORMAT_BINARY: u8 = 0x01;

// ─── TCP helpers ──────────────────────────────────────────────────────────────

/// Send a V1 request (text response).
async fn send(stream: &mut TcpStream, query: &str) -> Result<()> {
    let payload = query.as_bytes();
    stream.write_u8(V1).await?;
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Send a V2 request (binary response).
async fn send_bin(stream: &mut TcpStream, query: &str) -> Result<()> {
    let payload = query.as_bytes();
    stream.write_u8(V2).await?;
    stream.write_u8(FORMAT_BINARY).await?;
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Receive raw bytes response.
async fn recv_raw(stream: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    let status = stream.read_u8().await?;
    let length = stream.read_u32().await?;
    let mut buf = vec![0u8; length as usize];
    stream.read_exact(&mut buf).await?;
    Ok((status, buf))
}

async fn recv(stream: &mut TcpStream) -> Result<(u8, String)> {
    let status = stream.read_u8().await?;
    let length = stream.read_u32().await?;
    let mut buf = vec![0u8; length as usize];
    stream.read_exact(&mut buf).await?;
    Ok((status, String::from_utf8(buf)?))
}

async fn query(stream: &mut TcpStream, q: &str) -> Result<String> {
    send(stream, q).await?;
    let (status, payload) = recv(stream).await?;
    if status != STATUS_OK {
        bail!("Server error: {payload}");
    }
    Ok(payload)
}

/// Send query, receive binary QueryResult.
async fn query_bin(stream: &mut TcpStream, q: &str) -> Result<xyzdb_core::result::QueryResult> {
    send_bin(stream, q).await?;
    let (status, bytes) = recv_raw(stream).await?;
    if status != STATUS_OK {
        bail!("Server error: {}", String::from_utf8_lossy(&bytes));
    }
    let result: xyzdb_core::result::QueryResult =
        bincode::deserialize(&bytes).context("Failed to deserialize binary response")?;
    Ok(result)
}

async fn connect(host: &str, port: u16) -> Result<TcpStream> {
    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("Failed to connect to {addr}"))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

// ─── Data generation ──────────────────────────────────────────────────────────

const REGIONS: &[&str] = &[
    "West Coast",
    "East Coast",
    "Midwest",
    "Southeast",
    "Northeast",
    "Pacific",
    "Mountain",
    "Central",
    "Atlantic",
    "Southern",
];
const COUNTRIES: &[&str] = &[
    "US",
    "UK",
    "Germany",
    "Canada",
    "Australia",
    "France",
    "Japan",
    "Brazil",
    "India",
    "Netherlands",
];
const FIRST_NAMES: &[&str] = &[
    "James",
    "Emma",
    "Oliver",
    "Sophia",
    "Liam",
    "Ava",
    "Noah",
    "Mia",
    "Lucas",
    "Charlotte",
    "Ethan",
    "Amelia",
    "Mason",
    "Isabella",
];
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Miller", "Davis", "Wilson", "Taylor",
    "Clark", "Hall", "Walker",
];
const ROLES: &[&str] = &["Engineer", "Manager", "Analyst", "Consultant", "Freelancer"];
const CATEGORIES: &[&str] = &[
    "Internal",
    "External",
    "Research",
    "Infrastructure",
    "Product",
    "Operations",
];
const DIVISIONS: &[&str] = &["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Digital"];

fn pick<'a>(rng: &mut StdRng, list: &[&'a str]) -> &'a str {
    list.choose(rng).unwrap()
}

fn gen_company_query(rng: &mut StdRng, company_id: u32) -> String {
    let code = format!("COM-{company_id:06}");
    let first = pick(rng, FIRST_NAMES);
    let last = pick(rng, LAST_NAMES);
    let region = pick(rng, REGIONS);
    let country = pick(rng, COUNTRIES);
    let role = pick(rng, ROLES);
    let revenue: u32 = rng.random_range(8000..120000);
    let _score: u32 = rng.random_range(300..850);
    let y = rng.random_range(1965u32..2000);
    let m = rng.random_range(1u32..13);
    let d = rng.random_range(1u32..29);
    let zip: u32 = rng.random_range(1000..99999);
    let phone: u32 = rng.random_range(10000000..99999999);

    format!(
        r#"PUT {{_type: "Company", code: "{code}", name: "{first}", last_name: "{last}", founded: "{y}-{m:02}-{d:02}", email: "c{company_id}@mail.com", phone: "+1{phone}", address: "123 Main St", unit: "{}", district: "Downtown", zip: "{zip:05}", region: "{region}", country: "{country}", registered: "2025-01-15", status: "active", tier: "mid", revenue: {revenue}, role: "{role}"}} IN "catalog""#,
        rng.random_range(1u32..999)
    )
}

fn gen_project_query(rng: &mut StdRng, company_id: u32, project_idx: u32) -> (String, String) {
    let project_id = format!("PRJ-{company_id:06}-{project_idx}");
    let code = format!("COM-{company_id:06}");
    let budget: u32 = rng.random_range(10000..200000);
    let duration: u32 = *[12, 24, 36, 48].choose(rng).unwrap();
    let rate: f64 = rng.random_range(12..36) as f64 / 100.0;
    let category = pick(rng, CATEGORIES);
    let division = pick(rng, DIVISIONS);
    let ms = rng.random_range(1u32..13);

    let q = format!(
        r#"PUT {{_type: "Project", project_id: "{project_id}", category: "{category}", budget: {budget}, duration: {duration}, rate: {rate:.2}, start_date: "2025-{ms:02}-15", due_date: "2028-{ms:02}-15", status: "active", spent: 0, completed_budget: 0, review_day: 1, sync_day: 25, penalty: 0.0, insured: true, product: "{category}", division: "{division}", lead: "Auto", grade: "A", purpose: "delivery", collateral: "none"}} IN "catalog" LINK TO "catalog" WHERE code = "{code}" AS "owner""#
    );
    (q, project_id)
}

fn gen_task_query(
    rng: &mut StdRng,
    project_id: &str,
    task_num: u32,
    effort: f64,
    overhead: f64,
) -> String {
    let total = effort + overhead;
    let tax = overhead * 0.16;
    let month = ((task_num - 1) % 12) + 1;
    let status = if rng.random_bool(0.3) {
        "blocked"
    } else {
        "pending"
    };
    let days_overdue: u32 = if status == "blocked" {
        rng.random_range(1..90)
    } else {
        0
    };

    format!(
        r#"PUT {{_type: "Task", number: {task_num}, effort: {effort:.2}, overhead: {overhead:.2}, total: {total:.2}, due_date: "2026-{month:02}-25", deadline: "2026-{month:02}-28", status: "{status}", days_overdue: {days_overdue}, completed: 0.0, last_update: "", penalty_applied: 0.0, tax: {tax:.2}, fee: 0.0, remaining: {total:.2}, cadence: "monthly", task_type: "standard", reference: "", target_system: "Central", routing_key: "012345678901234567", label: "Task {task_num}"}} IN "catalog" LINK TO "catalog" WHERE project_id = "{project_id}" AS "task_of""#
    )
}

// ─── Latency stats ────────────────────────────────────────────────────────────

struct LatencyStats {
    latencies: Vec<Duration>,
}

impl LatencyStats {
    fn new() -> Self {
        Self {
            latencies: Vec::new(),
        }
    }

    fn with_capacity(cap: usize) -> Self {
        Self {
            latencies: Vec::with_capacity(cap),
        }
    }

    fn record(&mut self, d: Duration) {
        self.latencies.push(d);
    }

    fn percentiles(&mut self) -> (f64, f64, f64) {
        self.latencies.sort();
        let n = self.latencies.len();
        if n == 0 {
            return (0.0, 0.0, 0.0);
        }
        let p50 = self.latencies[n * 50 / 100].as_secs_f64() * 1000.0;
        let p95 = self.latencies[n * 95 / 100].as_secs_f64() * 1000.0;
        let p99 = self.latencies[n.saturating_sub(1).min(n * 99 / 100)].as_secs_f64() * 1000.0;
        (p50, p95, p99)
    }

    fn count(&self) -> usize {
        self.latencies.len()
    }

    fn total_secs(&self) -> f64 {
        self.latencies.iter().sum::<Duration>().as_secs_f64()
    }
}

fn format_num(n: u64) -> String {
    let s = n.to_string();
    let mut r = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            r.push(',');
        }
        r.push(c);
    }
    r.chars().rev().collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let run_all = args.test == "all";

    println!("\nxyzDB MVP Benchmark — Full Stack (TCP + Parser + Engine + Fjall)");
    println!(
        "Server: {}:{}  |  Companies: {}  |  Label: {}\n",
        args.host,
        args.port,
        format_num(args.clients as u64),
        args.label
    );

    // Estimate records
    let avg_projects: f64 = 1.5;
    let avg_tasks: f64 = 30.0;
    let est_records = args.clients as f64 * (1.0 + avg_projects + avg_projects * avg_tasks);
    println!("Estimated records: ~{}\n", format_num(est_records as u64));

    if run_all || args.test == "setup" {
        run_setup(&args).await?;
    }

    let mut codes: Vec<String> = Vec::new();
    let mut project_ids: Vec<String> = Vec::new();
    let mut total_records: u64 = 0;

    if run_all || args.test == "write" {
        let result = run_write(&args).await?;
        codes = result.0;
        project_ids = result.1;
        total_records = result.2;
    }

    if run_all || args.test == "find" {
        if codes.is_empty() {
            codes = (0..args.clients).map(|i| format!("COM-{i:06}")).collect();
        }
        run_find(&args, &codes).await?;
    }

    if run_all || args.test == "pull" {
        if codes.is_empty() {
            codes = (0..args.clients).map(|i| format!("COM-{i:06}")).collect();
        }
        run_pull(&args, &codes).await?;
    }

    if run_all || args.test == "scan" {
        run_scan(&args, total_records).await?;
    }

    if run_all || args.test == "mixed" {
        if codes.is_empty() {
            codes = (0..args.clients).map(|i| format!("COM-{i:06}")).collect();
        }
        if project_ids.is_empty() {
            project_ids = (0..args.clients)
                .flat_map(|i| (0..2).map(move |j| format!("PRJ-{i:06}-{j}")))
                .collect();
        }
        run_mixed(&args, &codes, &project_ids).await?;
    }

    if run_all || args.test == "mutations" {
        if codes.is_empty() {
            codes = (0..args.clients).map(|i| format!("COM-{i:06}")).collect();
        }
        run_mutations(&args, &codes).await?;
    }

    println!("══════════════════════════════════════════════════════════════════════");
    println!("  Benchmark complete.");
    println!("══════════════════════════════════════════════════════════════════════\n");

    Ok(())
}

// ─── Setup ────────────────────────────────────────────────────────────────────

async fn run_setup(args: &Args) -> Result<()> {
    println!("── SETUP ──────────────────────────────────────────────────────────\n");
    let mut stream = connect(&args.host, args.port).await?;

    query(&mut stream, r#"LOBE "catalog""#).await?;
    println!("  Lobe 'catalog' created");
    query(&mut stream, r#"ANCHOR "code" UNIQUE IN "catalog""#).await?;
    println!("  Anchor 'code' registered");
    query(&mut stream, r#"ANCHOR "project_id" UNIQUE IN "catalog""#).await?;
    println!("  Anchor 'project_id' registered");
    println!("  Setup complete.\n");
    Ok(())
}

// ─── Test 1: Write ────────────────────────────────────────────────────────────

async fn run_write(args: &Args) -> Result<(Vec<String>, Vec<String>, u64)> {
    println!("── TEST 1: WRITE THROUGHPUT ────────────────────────────────────────\n");

    // 1a: Sequential
    println!("  1a. Sequential PUT (1 connection)...");
    let mut stream = connect(&args.host, args.port).await?;
    let mut rng = StdRng::seed_from_u64(42);
    let mut codes = Vec::with_capacity(args.clients as usize);
    let mut project_ids = Vec::new();
    let mut total_records: u64 = 0;
    let mut lat = LatencyStats::new();

    let wall_start = Instant::now();
    let mut last_report = Instant::now();
    let mut window_ops: u64 = 0;

    for company_id in 0..args.clients {
        let code = format!("COM-{company_id:06}");
        codes.push(code.clone());

        // Company
        let q = gen_company_query(&mut rng, company_id);
        let start = Instant::now();
        query(&mut stream, &q).await?;
        lat.record(start.elapsed());
        total_records += 1;
        window_ops += 1;

        // Projects
        let num_projects: u32 = rng.random_range(1..=2);
        for pi in 0..num_projects {
            let (q, pid) = gen_project_query(&mut rng, company_id, pi);
            project_ids.push(pid.clone());

            let start = Instant::now();
            query(&mut stream, &q).await?;
            lat.record(start.elapsed());
            total_records += 1;
            window_ops += 1;

            // Tasks
            let duration: u32 = *[12, 24, 36].choose(&mut rng).unwrap();
            let budget: f64 = rng.random_range(10000.0..200000.0);
            let effort = budget / duration as f64;
            let overhead = (budget * 0.24) / 12.0;

            for task_num in 1..=duration {
                let q = gen_task_query(&mut rng, &pid, task_num, effort, overhead);
                let start = Instant::now();
                query(&mut stream, &q).await?;
                lat.record(start.elapsed());
                total_records += 1;
                window_ops += 1;
            }
        }

        // Window report every 10s
        if last_report.elapsed() >= Duration::from_secs(10) {
            let elapsed = last_report.elapsed().as_secs_f64();
            let ops_s = window_ops as f64 / elapsed;
            println!(
                "    [{:>6.0}s] {} ops/s  ({} records so far)",
                wall_start.elapsed().as_secs_f64(),
                format_num(ops_s as u64),
                format_num(total_records)
            );
            last_report = Instant::now();
            window_ops = 0;
        }
    }

    let wall_time = wall_start.elapsed();
    let seq_ops = total_records as f64 / wall_time.as_secs_f64();
    let (p50, p95, p99) = lat.percentiles();

    println!("\n  Test 1a — Sequential PUT");
    println!("  {:30} {}", "Total records:", format_num(total_records));
    println!("  {:30} {:.1} s", "Total time:", wall_time.as_secs_f64());
    println!(
        "  {:30} {} ops/s",
        "Throughput:",
        format_num(seq_ops as u64)
    );
    println!("  {:30} {:.3} ms", "Latency P50:", p50);
    println!("  {:30} {:.3} ms", "Latency P95:", p95);
    println!("  {:30} {:.3} ms", "Latency P99:", p99);
    println!("  {:30} POC 2 did 476K (SSD raw Fjall)", "Comparison:");
    println!(
        "  {:30} {:.1}x overhead\n",
        "Stack factor:",
        476_000.0 / seq_ops
    );

    // 1c: Concurrent (4 connections) — insert extra records
    println!("  1c. Concurrent PUT (4 connections, 10K companies each)...");
    let concurrent_clients = 10_000u32;
    let threads = 4u32;
    let per_thread = concurrent_clients / threads;
    let conc_total = Arc::new(AtomicU64::new(0));
    let conc_start = Instant::now();

    let mut handles = Vec::new();
    for tid in 0..threads {
        let host = args.host.clone();
        let port = args.port;
        let total = conc_total.clone();
        let base = args.clients + tid * per_thread;

        handles.push(tokio::spawn(async move {
            let mut stream = connect(&host, port).await.expect("connect");
            let mut rng = StdRng::seed_from_u64(1000 + tid as u64);
            let mut ops: u64 = 0;

            for company_id in base..(base + per_thread) {
                let q = gen_company_query(&mut rng, company_id);
                let _ = query(&mut stream, &q).await;
                ops += 1;

                let num_projects: u32 = rng.random_range(1..=2);
                for pi in 0..num_projects {
                    let (q, pid) = gen_project_query(&mut rng, company_id, pi);
                    let _ = query(&mut stream, &q).await;
                    ops += 1;

                    let duration: u32 = *[12, 24, 36].choose(&mut rng).unwrap();
                    let budget: f64 = rng.random_range(10000.0..200000.0);
                    let effort = budget / duration as f64;
                    let overhead = (budget * 0.24) / 12.0;

                    for task_num in 1..=duration {
                        let q = gen_task_query(&mut rng, &pid, task_num, effort, overhead);
                        let _ = query(&mut stream, &q).await;
                        ops += 1;
                    }
                }
            }
            total.fetch_add(ops, Ordering::Relaxed);
        }));
    }

    for h in handles {
        h.await?;
    }

    let conc_time = conc_start.elapsed();
    let conc_ops = conc_total.load(Ordering::Relaxed);
    let conc_throughput = conc_ops as f64 / conc_time.as_secs_f64();

    println!("  Test 1c — Concurrent PUT ({threads} connections)");
    println!("  {:30} {}", "Total records:", format_num(conc_ops));
    println!("  {:30} {:.1} s", "Total time:", conc_time.as_secs_f64());
    println!(
        "  {:30} {} ops/s",
        "Throughput:",
        format_num(conc_throughput as u64)
    );
    println!(
        "  {:30} {} ops/s\n",
        "Per connection:",
        format_num((conc_throughput / threads as f64) as u64)
    );

    // 1d: Batch PUT — insert 5K companies with 36 tasks each via PUT BATCH
    println!("  1d. Batch PUT (batches of ~36, 5K companies)...");
    let batch_clients = 5_000u32;
    let mut batch_stream = connect(&args.host, args.port).await?;
    let mut batch_rng = StdRng::seed_from_u64(9999);
    let batch_start = Instant::now();
    let mut batch_records: u64 = 0;

    let base_id = args.clients + concurrent_clients * threads + 1000;
    for company_id in base_id..(base_id + batch_clients) {
        // Company (single PUT)
        let q = gen_company_query(&mut batch_rng, company_id);
        query(&mut batch_stream, &q).await?;
        batch_records += 1;

        // Project (single PUT with LINK)
        let (q, pid) = gen_project_query(&mut batch_rng, company_id, 0);
        query(&mut batch_stream, &q).await?;
        batch_records += 1;

        // Tasks as BATCH
        let duration: u32 = *[24, 36].choose(&mut batch_rng).unwrap();
        let budget: f64 = batch_rng.random_range(10000.0..200000.0);
        let effort = budget / duration as f64;
        let overhead = (budget * 0.24) / 12.0;

        let mut batch_q = r#"PUT BATCH IN "catalog" ["#.to_string();
        for task in 1..=duration {
            if task > 1 {
                batch_q.push_str(", ");
            }
            let total = effort + overhead;
            let tax = overhead * 0.16;
            let month = ((task - 1) % 12) + 1;
            batch_q.push_str(&format!(
                r#"{{_type: "Task", number: {task}, effort: {effort:.2}, overhead: {overhead:.2}, total: {total:.2}, due_date: "2026-{month:02}-25", status: "pending", tax: {tax:.2}, label: "Task {task}"}}"#
            ));
        }
        batch_q.push_str(&format!(
            r#"] LINK TO "catalog" WHERE project_id = "{pid}" AS "task_of""#
        ));
        query(&mut batch_stream, &batch_q).await?;
        batch_records += duration as u64;
    }

    let batch_time = batch_start.elapsed();
    let batch_throughput = batch_records as f64 / batch_time.as_secs_f64();

    println!("  Test 1d — Batch PUT (tasks batched per project)");
    println!("  {:30} {}", "Total records:", format_num(batch_records));
    println!("  {:30} {:.1} s", "Total time:", batch_time.as_secs_f64());
    println!(
        "  {:30} {} ops/s",
        "Throughput:",
        format_num(batch_throughput as u64)
    );
    println!(
        "  {:30} {:.1}x overhead vs POC\n",
        "Stack factor:",
        476_000.0 / batch_throughput
    );

    Ok((codes, project_ids, total_records))
}

// ─── Test 2: FIND ─────────────────────────────────────────────────────────────

async fn run_find(args: &Args, codes: &[String]) -> Result<()> {
    println!("── TEST 2: FIND (Anchor Lookup) ───────────────────────────────────\n");

    let sample_size = 10_000usize.min(codes.len());
    let mut rng = StdRng::seed_from_u64(99);
    let mut sample: Vec<&String> = codes.iter().collect();
    sample.shuffle(&mut rng);
    let sample = &sample[..sample_size];

    // 2a: Sequential
    println!(
        "  2a. Sequential FIND ({} queries, 1 connection)...",
        format_num(sample_size as u64)
    );
    let mut stream = connect(&args.host, args.port).await?;

    // Warmup
    for code in sample.iter().take(WARMUP) {
        let _ = query(
            &mut stream,
            &format!(r#"FIND "catalog" WHERE code = "{code}""#),
        )
        .await;
    }

    let mut lat = LatencyStats::with_capacity(sample_size);
    for code in sample {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}""#);
        let start = Instant::now();
        query(&mut stream, &q).await?;
        lat.record(start.elapsed());
    }

    let (p50, _p95, p99) = lat.percentiles();
    let throughput = lat.count() as f64 / lat.total_secs();
    println!("  {:30} {:.3} ms", "P50:", p50);
    println!("  {:30} {:.3} ms", "P99:", p99);
    println!(
        "  {:30} {} finds/s\n",
        "Throughput:",
        format_num(throughput as u64)
    );

    // 2b: Concurrent
    println!(
        "  2b. Concurrent FIND (4 connections, {} total)...",
        format_num(sample_size as u64)
    );
    let threads = 4u32;
    let per_thread = sample_size / threads as usize;
    let shared_codes: Arc<Vec<String>> = Arc::new(sample.iter().map(|s| (*s).clone()).collect());
    let conc_start = Instant::now();
    let conc_count = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for tid in 0..threads {
        let codes = shared_codes.clone();
        let host = args.host.clone();
        let port = args.port;
        let count = conc_count.clone();

        handles.push(tokio::spawn(async move {
            let mut stream = connect(&host, port).await.expect("connect");
            let start = tid as usize * per_thread;
            let end = start + per_thread;
            for code in &codes[start..end] {
                let q = format!(r#"FIND "catalog" WHERE code = "{code}""#);
                let _ = query(&mut stream, &q).await;
                count.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.await?;
    }

    let conc_time = conc_start.elapsed();
    let conc_throughput = conc_count.load(Ordering::Relaxed) as f64 / conc_time.as_secs_f64();
    println!(
        "  {:30} {} finds/s\n",
        "Throughput:",
        format_num(conc_throughput as u64)
    );

    // 2c: Repeated (cache warmup)
    println!("  2c. Repeated FIND (same code, 1000x)...");
    let target_code = &codes[0];
    let q = format!(r#"FIND "catalog" WHERE code = "{target_code}""#);

    let first_start = Instant::now();
    query(&mut stream, &q).await?;
    let first = first_start.elapsed();

    for _ in 0..998 {
        query(&mut stream, &q).await?;
    }
    let last_start = Instant::now();
    query(&mut stream, &q).await?;
    let last = last_start.elapsed();

    println!(
        "  {:30} {:.3} ms",
        "1st (cold):",
        first.as_secs_f64() * 1000.0
    );
    println!(
        "  {:30} {:.3} ms",
        "1000th (warm):",
        last.as_secs_f64() * 1000.0
    );
    println!(
        "  {:30} {:.1}x\n",
        "Speedup:",
        first.as_secs_f64() / last.as_secs_f64()
    );

    Ok(())
}

// ─── Test 3: PULL ─────────────────────────────────────────────────────────────

async fn run_pull(args: &Args, codes: &[String]) -> Result<()> {
    println!("── TEST 3: PULL (The Money Shot) ────────────────────────────────\n");

    let sample_size = 1_000usize.min(codes.len());
    let mut rng = StdRng::seed_from_u64(77);
    let mut sample: Vec<&String> = codes.iter().collect();
    sample.shuffle(&mut rng);
    let sample = &sample[..sample_size];

    let mut stream = connect(&args.host, args.port).await?;

    // 3a: "Cold" PULL (first pass after ingestion — block cache cold for these keys)
    println!(
        "  3a. Cold PULL ({} companies)...",
        format_num(sample_size as u64)
    );

    let mut lat_cold = LatencyStats::with_capacity(sample_size);
    let mut total_records: u64 = 0;

    for code in sample {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}" | PULL depth=1"#);
        let start = Instant::now();
        let resp = query(&mut stream, &q).await?;
        lat_cold.record(start.elapsed());
        total_records += resp.matches("LID:").count() as u64;
    }

    let avg_records = total_records as f64 / sample_size as f64;
    let (p50, _p95, p99) = lat_cold.percentiles();
    let throughput = lat_cold.count() as f64 / lat_cold.total_secs();

    println!("  {:30} {:.1}", "Avg records/PULL:", avg_records);
    println!("  {:30} {:.3} ms", "P50:", p50);
    println!("  {:30} {:.3} ms", "P99:", p99);
    println!(
        "  {:30} {} pulls/s",
        "Throughput:",
        format_num(throughput as u64)
    );
    println!(
        "  {:30} POC 2: 0.241ms SSD at 123M records\n",
        "Comparison:"
    );

    // 3b: Warm PULL (same companies again)
    println!(
        "  3b. Warm PULL ({} companies, cache primed)...",
        format_num(sample_size as u64)
    );

    let mut lat_warm = LatencyStats::with_capacity(sample_size);
    for code in sample {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}" | PULL depth=1"#);
        let start = Instant::now();
        query(&mut stream, &q).await?;
        lat_warm.record(start.elapsed());
    }

    let (p50w, _p95w, p99w) = lat_warm.percentiles();
    let warm_throughput = lat_warm.count() as f64 / lat_warm.total_secs();
    println!("  {:30} {:.3} ms", "P50:", p50w);
    println!("  {:30} {:.3} ms", "P99:", p99w);
    println!(
        "  {:30} {} pulls/s",
        "Throughput:",
        format_num(warm_throughput as u64)
    );
    println!("  {:30} {:.1}x\n", "Speedup vs cold:", p50 / p50w);

    // 3c: Concurrent PULL
    println!(
        "  3c. Concurrent PULL (4 connections, {} total)...",
        format_num(sample_size as u64)
    );
    let threads = 4u32;
    let per_thread = sample_size / threads as usize;
    let shared_codes: Arc<Vec<String>> = Arc::new(sample.iter().map(|s| (*s).clone()).collect());
    let conc_start = Instant::now();
    let conc_count = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for tid in 0..threads {
        let codes = shared_codes.clone();
        let host = args.host.clone();
        let port = args.port;
        let count = conc_count.clone();

        handles.push(tokio::spawn(async move {
            let mut stream = connect(&host, port).await.expect("connect");
            let start_idx = tid as usize * per_thread;
            let end_idx = start_idx + per_thread;
            for code in &codes[start_idx..end_idx] {
                let q = format!(r#"FIND "catalog" WHERE code = "{code}" | PULL depth=1"#);
                let _ = query(&mut stream, &q).await;
                count.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.await?;
    }

    let conc_time = conc_start.elapsed();
    let conc_throughput = conc_count.load(Ordering::Relaxed) as f64 / conc_time.as_secs_f64();
    println!(
        "  {:30} {} pulls/s\n",
        "Throughput:",
        format_num(conc_throughput as u64)
    );

    // 3d: PULL only=Task
    println!("  3d. PULL only=Task (100 companies)...");
    let mut lat_only = LatencyStats::with_capacity(100);
    let mut only_records: u64 = 0;

    for code in sample.iter().take(100) {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}" | PULL only=Task"#);
        let start = Instant::now();
        let resp = query(&mut stream, &q).await?;
        lat_only.record(start.elapsed());
        only_records += resp.matches("LID:").count() as u64;
    }

    let (p50o, _, _) = lat_only.percentiles();
    println!(
        "  {:30} {:.1}",
        "Avg records (only Task):",
        only_records as f64 / 100.0
    );
    println!("  {:30} {:.3} ms\n", "P50:", p50o);

    // 3e: Binary PULL (bincode response instead of text)
    println!("  3e. Binary PULL (1,000 companies, bincode response)...");
    let mut lat_bin = LatencyStats::with_capacity(sample_size);
    let mut bin_records: u64 = 0;

    for code in sample {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}" | PULL depth=1"#);
        let start = Instant::now();
        let result = query_bin(&mut stream, &q).await?;
        lat_bin.record(start.elapsed());
        if let xyzdb_core::result::QueryResult::Records(recs) = &result {
            bin_records += recs.len() as u64;
        }
    }

    let (p50b, _, p99b) = lat_bin.percentiles();
    let bin_throughput = lat_bin.count() as f64 / lat_bin.total_secs();
    println!(
        "  {:30} {:.1}",
        "Avg records/PULL:",
        bin_records as f64 / sample_size as f64
    );
    println!("  {:30} {:.3} ms", "P50:", p50b);
    println!("  {:30} {:.3} ms", "P99:", p99b);
    println!(
        "  {:30} {} pulls/s",
        "Throughput:",
        format_num(bin_throughput as u64)
    );
    println!("  {:30} {:.1}x", "vs text warm P50:", p50w / p50b);
    println!();

    Ok(())
}

// ─── Test 4: SCAN ─────────────────────────────────────────────────────────────

async fn run_scan(args: &Args, total_records: u64) -> Result<()> {
    println!("── TEST 4: SCAN (Cross-Entity) ──────────────────────────────────\n");

    let mut stream = connect(&args.host, args.port).await?;

    // 4a: Selective scan — projects with high budget
    println!("  4a. SCAN Projects with budget > 190000 (selective)...");
    let start = Instant::now();
    let resp = query(
        &mut stream,
        r#"SCAN "catalog" WHERE _type = "Project" AND budget > 190000"#,
    )
    .await?;
    let elapsed = start.elapsed();
    let matched = resp.matches("LID:").count();
    println!("  {:30} {:.3} s", "Time:", elapsed.as_secs_f64());
    println!("  {:30} {}", "Records matched:", format_num(matched as u64));
    if total_records > 0 {
        let scan_throughput = total_records as f64 / elapsed.as_secs_f64();
        println!(
            "  {:30} {} rec/s (estimated)",
            "Scan throughput:",
            format_num(scan_throughput as u64)
        );
    }

    // 4b: Typed scan — blocked tasks
    println!("\n  4b. SCAN blocked Tasks (status=\"blocked\")...");
    let start = Instant::now();
    let resp = query(
        &mut stream,
        r#"SCAN "catalog" WHERE _type = "Task" AND status = "blocked""#,
    )
    .await?;
    let elapsed = start.elapsed();
    let matched = resp.matches("LID:").count();
    println!("  {:30} {:.3} s", "Time:", elapsed.as_secs_f64());
    println!(
        "  {:30} {}\n",
        "Records matched:",
        format_num(matched as u64)
    );

    Ok(())
}

// ─── Test 5: Mixed ────────────────────────────────────────────────────────────

async fn run_mixed(args: &Args, codes: &[String], project_ids: &[String]) -> Result<()> {
    println!("── TEST 5: MIXED WORKLOAD (2W + 4R, 60s) ────────────────────────\n");

    let duration = Duration::from_secs(60);
    let stop = Arc::new(AtomicBool::new(false));
    let write_ops = Arc::new(AtomicU64::new(0));
    let pull_ops = Arc::new(AtomicU64::new(0));

    let shared_codes: Arc<Vec<String>> = Arc::new(codes.to_vec());
    let shared_projects: Arc<Vec<String>> = Arc::new(project_ids.to_vec());

    let mut handles = Vec::new();

    // 2 writer threads (comments)
    for tid in 0..2u32 {
        let host = args.host.clone();
        let port = args.port;
        let stop = stop.clone();
        let ops = write_ops.clone();
        let projects = shared_projects.clone();

        handles.push(tokio::spawn(async move {
            let mut stream = connect(&host, port).await.expect("connect");
            let mut rng = StdRng::seed_from_u64(5000 + tid as u64);
            let mut i: u64 = 0;

            while !stop.load(Ordering::Relaxed) {
                if projects.is_empty() { break; }
                let pid = &projects[rng.random_range(0..projects.len())];
                let amount: u32 = rng.random_range(500..5000);
                let q = format!(
                    r#"PUT {{_type: "Comment", amount: {amount}, date: "2026-03-27", method: "API", reference: "CMT-{tid}-{i}", channel: "Web"}} IN "catalog" LINK TO "catalog" WHERE project_id = "{pid}" AS "comment_on""#
                );
                let _ = query(&mut stream, &q).await;
                ops.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        }));
    }

    // 4 PULL threads
    for tid in 0..4u32 {
        let host = args.host.clone();
        let port = args.port;
        let stop = stop.clone();
        let ops = pull_ops.clone();
        let codes = shared_codes.clone();

        handles.push(tokio::spawn(async move {
            let mut stream = connect(&host, port).await.expect("connect");
            let mut rng = StdRng::seed_from_u64(7000 + tid as u64);

            while !stop.load(Ordering::Relaxed) {
                if codes.is_empty() {
                    break;
                }
                let code = &codes[rng.random_range(0..codes.len())];
                let q = format!(r#"FIND "catalog" WHERE code = "{code}" | PULL depth=1"#);
                let _ = query(&mut stream, &q).await;
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Reporter
    let report_stop = stop.clone();
    let report_writes = write_ops.clone();
    let report_pulls = pull_ops.clone();

    let reporter = tokio::spawn(async move {
        let mut last_w: u64 = 0;
        let mut last_p: u64 = 0;
        let mut tick = 0u32;

        while !report_stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(10)).await;
            tick += 1;
            let w = report_writes.load(Ordering::Relaxed);
            let p = report_pulls.load(Ordering::Relaxed);
            let dw = w - last_w;
            let dp = p - last_p;
            println!(
                "    [{:>3}s] PUT: {} ops/s | PULL: {} ops/s",
                tick * 10,
                format_num(dw / 10),
                format_num(dp / 10)
            );
            last_w = w;
            last_p = p;
        }
    });

    tokio::time::sleep(duration).await;
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        let _ = h.await;
    }
    reporter.abort();

    let total_w = write_ops.load(Ordering::Relaxed);
    let total_p = pull_ops.load(Ordering::Relaxed);
    let w_rate = total_w as f64 / duration.as_secs_f64();
    let p_rate = total_p as f64 / duration.as_secs_f64();

    println!("\n  Overall (60s):");
    println!(
        "  {:30} {} ops/s",
        "PUT throughput:",
        format_num(w_rate as u64)
    );
    println!(
        "  {:30} {} pulls/s\n",
        "PULL throughput:",
        format_num(p_rate as u64)
    );

    Ok(())
}

// ─── Test 6: Mutations ────────────────────────────────────────────────────────

async fn run_mutations(args: &Args, codes: &[String]) -> Result<()> {
    println!("── TEST 6: MUTATIONS (SET + DELETE) ──────────────────────────────\n");

    let mut stream = connect(&args.host, args.port).await?;
    let mut rng = StdRng::seed_from_u64(333);

    let sample_size = 1000usize.min(codes.len());
    let mut sample: Vec<&String> = codes.iter().collect();
    sample.shuffle(&mut rng);

    // SET: update status on companies
    println!("  SET ({} operations)...", format_num(sample_size as u64));
    let mut lat_set = LatencyStats::with_capacity(sample_size);
    for code in sample.iter().take(sample_size) {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}" | SET status = "reviewed""#);
        let start = Instant::now();
        let _ = query(&mut stream, &q).await;
        lat_set.record(start.elapsed());
    }

    let (p50s, _, p99s) = lat_set.percentiles();
    println!("  {:30} {:.3} ms", "SET P50:", p50s);
    println!("  {:30} {:.3} ms\n", "SET P99:", p99s);

    // DELETE: delete 100 companies (from end of range to not affect other tests)
    let delete_count = 100usize.min(codes.len());
    println!(
        "  DELETE ({} operations)...",
        format_num(delete_count as u64)
    );
    let mut lat_del = LatencyStats::with_capacity(delete_count);
    for code in sample.iter().rev().take(delete_count) {
        let q = format!(r#"FIND "catalog" WHERE code = "{code}" | DELETE"#);
        let start = Instant::now();
        let _ = query(&mut stream, &q).await;
        lat_del.record(start.elapsed());
    }

    let (p50d, _, p99d) = lat_del.percentiles();
    println!("  {:30} {:.3} ms", "DELETE P50:", p50d);
    println!("  {:30} {:.3} ms\n", "DELETE P99:", p99d);

    Ok(())
}
