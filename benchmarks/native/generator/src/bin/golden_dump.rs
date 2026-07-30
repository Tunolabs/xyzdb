//! v0.3.4 Phase E — golden file emitter.
//!
//! Walks the deterministic generator iterators once and writes the V1-V6
//! aggregates to `<out-dir>/golden-scale<X>-seed<Y>.json`. The orchestrator
//! and each driver's `verify_golden` later read this file and compare
//! their engine-side observed values against it.
//!
//! Usage:
//!     cargo run --release -p native-generator --bin golden_dump -- \
//!         --scale 0.1 --seed 42 --out-dir ./results

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use native_generator::Dataset;

fn main() -> Result<()> {
    let mut scale: f64 = 0.1;
    let mut seed: u64 = 42;
    let mut out_dir: PathBuf = PathBuf::from("./results");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--scale" => {
                scale = it
                    .next()
                    .context("--scale needs a value")?
                    .parse()
                    .context("--scale must parse as f64")?;
            }
            "--seed" => {
                seed = it
                    .next()
                    .context("--seed needs a value")?
                    .parse()
                    .context("--seed must parse as u64")?;
            }
            "--out-dir" => {
                out_dir = PathBuf::from(it.next().context("--out-dir needs a value")?);
            }
            "--help" | "-h" => {
                eprintln!(
                    "golden_dump --scale <f64> --seed <u64> --out-dir <path>\n\
                     Defaults: scale=0.1 seed=42 out-dir=./results"
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    std::fs::create_dir_all(&out_dir).with_context(|| format!("create {:?}", out_dir))?;
    let path = out_dir.join(format!("golden-scale{}-seed{}.json", scale, seed));

    let now = Utc::now();
    eprintln!(
        "computing golden aggregates: seed={} scale={} reference_now={}",
        seed,
        scale,
        now.to_rfc3339()
    );

    let ds = Dataset::new(seed, scale);
    let golden = ds.compute_golden_aggregates(now);

    let json = serde_json::to_string_pretty(&golden).context("serialise golden")?;
    std::fs::write(&path, json).with_context(|| format!("write {:?}", path))?;

    eprintln!("wrote {:?}", path);
    eprintln!(
        "V1 credits        n={} sum={:.2}",
        golden.verify_queries.v1_credits_total.n, golden.verify_queries.v1_credits_total.sum
    );
    eprintln!(
        "V2 inst overdue   n={} sum={:.2}",
        golden.verify_queries.v2_installments_overdue.n,
        golden.verify_queries.v2_installments_overdue.sum
    );
    eprintln!(
        "V3 payments       n={} sum={:.2}",
        golden.verify_queries.v3_payments_total.n, golden.verify_queries.v3_payments_total.sum
    );
    eprintln!(
        "V5 clients rfc    n={}",
        golden.verify_queries.v5_clients_distinct_rfc.n
    );
    eprintln!(
        "V6 config         empresas={} productos={} total={}",
        golden.verify_queries.v6_config_counts.empresas,
        golden.verify_queries.v6_config_counts.productos,
        golden.verify_queries.v6_config_counts.total
    );

    Ok(())
}
