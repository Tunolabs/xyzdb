//! Sidecar microbench — isolates the materialization cost that dominates NEAREST.
//!
//! Uses the REAL production codecs (`serialize_record`/`deserialize_record`) and the
//! REAL distance path (`as_vector` widen-to-f64 + `similarity`) so Path A faithfully
//! reproduces the engine's in-process NEAREST cost (anchor: ~30-38 ms end-to-end over
//! TCP at an 8k bucket; this excludes only TCP, which is identical for both paths).
//!
//! Path A (today): deserialize EVERY full record blob (vec + 3 KB text + meta) →
//!   `as_vector` (f32→f64 alloc) → cosine → top-k min-heap.
//! Path B (sidecar): SIMD-friendly cosine over a contiguous dense f32 column built
//!   once → top-k indices → deserialize ONLY the top-k full records.
//!
//! Run: cargo run --release --example sidecar_bench -p xyzdb-core
//!   env: N (bucket size, default 8000), DIM (default 256), TEXT (bytes, default 3000),
//!        K (top-k, default 10), ITERS (queries timed, default 100).

// SPDX-License-Identifier: BUSL-1.1
use std::collections::BTreeMap;
use std::time::Instant;

use xyzdb_core::distance::{self, Metric};
use xyzdb_core::lid::LID;
use xyzdb_core::record::{Record, deserialize_record, serialize_record};
use xyzdb_core::value::Value;

/// Deterministic LCG → f32 in [-1, 1]; no rand dependency, reproducible.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

fn unit_vec(lcg: &mut Lcg, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim).map(|_| lcg.next_f32()).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

/// f32 cosine — simple loop; opt-level=3 autovectorizes. Real SIMD intrinsics ≥ this.
fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return -1.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn pctl(mut xs: Vec<f64>, q: f64) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[((xs.len() as f64 * q) as usize).min(xs.len() - 1)]
}

fn main() {
    let n = env_usize("N", 8000);
    let dim = env_usize("DIM", 256);
    let text_bytes = env_usize("TEXT", 3000);
    let k = env_usize("K", 10);
    let iters = env_usize("ITERS", 100);

    let mut lcg = Lcg(0x1234_5678_9abc_def0);
    let text: String = "the quick brown fox jumps over the lazy dog and remembers everything "
        .chars()
        .cycle()
        .take(text_bytes)
        .collect();

    // Build the bucket: N records (vec + text + meta), serialize to real on-disk blobs.
    let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut dense: Vec<f32> = Vec::with_capacity(n * dim); // the sidecar column
    for i in 0..n {
        let v = unit_vec(&mut lcg, dim);
        dense.extend_from_slice(&v);
        let mut fields: BTreeMap<String, Value> = BTreeMap::new();
        fields.insert("vec".into(), Value::Vector(v));
        fields.insert("txt".into(), Value::Text(text.clone()));
        fields.insert("ts".into(), Value::Int(i as i64));
        let rec = Record {
            lid: LID::from_raw(i as u128),
            lobe_name: "mem".into(),
            fields,
            created_at: i as i64,
            updated_at: i as i64,
        };
        blobs.push(serialize_record(&rec));
    }
    let blob_bytes: usize = blobs.iter().map(|b| b.len()).sum();
    eprintln!(
        "bucket N={n} dim={dim} text={text_bytes}B → {} MB of row blobs; dense sidecar = {} MB",
        blob_bytes / 1_048_576,
        dense.len() * 4 / 1_048_576
    );

    let queries: Vec<Vec<f32>> = (0..iters).map(|_| unit_vec(&mut lcg, dim)).collect();

    // ── Path A: current NEAREST (deserialize ALL, as_vector, f32 SIMD similarity) ──
    let mut a_ms = Vec::with_capacity(iters);
    let mut checksum_a = 0u128;
    for q in &queries {
        let t = Instant::now();
        // min-heap of (score, idx); keep top-k
        let mut heap: Vec<(f64, usize)> = Vec::with_capacity(k + 1);
        for (idx, blob) in blobs.iter().enumerate() {
            let rec = deserialize_record(blob, "mem", None).unwrap();
            let v = match rec.fields.get("vec").and_then(distance::as_vector) {
                Some(v) => v,
                None => continue,
            };
            let score =
                distance::similarity(Metric::Cosine, q, bytemuck::cast_slice(&v)).unwrap_or(-1.0);
            heap.push((score, idx));
            heap.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            heap.truncate(k);
        }
        a_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        checksum_a = checksum_a.wrapping_add(heap[0].1 as u128);
    }

    // ── Path B: sidecar (dense f32 cosine over all, then deserialize ONLY top-k) ──
    let mut b_ms = Vec::with_capacity(iters);
    let mut checksum_b = 0u128;
    for q in &queries {
        let t = Instant::now();
        let mut heap: Vec<(f32, usize)> = Vec::with_capacity(k + 1);
        for idx in 0..n {
            let v = &dense[idx * dim..(idx + 1) * dim];
            let score = cosine_f32(q, v);
            heap.push((score, idx));
            heap.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            heap.truncate(k);
        }
        // materialize ONLY the winners (what NEAREST would actually return)
        let winners: Vec<Record> = heap
            .iter()
            .map(|(_, idx)| deserialize_record(&blobs[*idx], "mem", None).unwrap())
            .collect();
        b_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        checksum_b = checksum_b.wrapping_add(winners[0].lid.raw());
    }

    println!(
        "\n=== sidecar bench (N={n}, dim={dim}, text={text_bytes}B, k={k}, iters={iters}) ==="
    );
    println!("Path A (current NEAREST: deserialize all {n}):");
    println!(
        "  p50={:.3}ms  p99={:.3}ms  mean={:.3}ms",
        pctl(a_ms.clone(), 0.5),
        pctl(a_ms.clone(), 0.99),
        a_ms.iter().sum::<f64>() / iters as f64
    );
    println!("Path B (sidecar: dense f32 sweep + deserialize {k}):");
    println!(
        "  p50={:.3}ms  p99={:.3}ms  mean={:.3}ms",
        pctl(b_ms.clone(), 0.5),
        pctl(b_ms.clone(), 0.99),
        b_ms.iter().sum::<f64>() / iters as f64
    );
    let sa = pctl(a_ms, 0.5);
    let sb = pctl(b_ms, 0.5);
    println!(
        "speedup p50: {:.1}x   (records materialized: {} → {})",
        sa / sb,
        n,
        k
    );
    println!("(checksums {checksum_a} {checksum_b} — keep optimizer honest)");
}
