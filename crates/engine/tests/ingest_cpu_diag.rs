//! Diagnostic (not a correctness test): where does vector-ingest CPU go?
//! Isolates text-parse (V1 nom) vs execute (put) vs binary-bulk (V3, no parse). Same type
//! stored (Value::Vector f32) on both paths. Run: cargo test --release -p xyzdb-engine
//! --test ingest_cpu_diag -- --nocapture  (release: debug does not reflect the real cost).
use std::collections::BTreeMap;
use std::time::Instant;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::Engine;
use xyzdb_engine::ops::put::{BulkRecord, execute_bulk_insert};

const B: usize = 2000; // records per batch
const DIM: usize = 768;

fn mkvec(i: usize) -> Vec<f32> {
    // deterministic, cheap; the content does not matter for the cost
    (0..DIM)
        .map(|d| ((i * 31 + d * 7) % 1000) as f32 * 0.001 - 0.5)
        .collect()
}

#[ignore = "diagnostic, not a correctness test; run on demand: cargo test --release ... -- --ignored --nocapture"]
#[test]
fn ingest_cpu_breakdown() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::open(tmp.path()).unwrap();
    engine
        .execute(xytalk_parser::parse("LOBE \"v1\"").unwrap())
        .unwrap();
    engine
        .execute(xytalk_parser::parse("LOBE \"v3\"").unwrap())
        .unwrap();

    // --- Build the V1 text statement (PUT BATCH with B vectors) ---
    let mut text = String::from("PUT BATCH IN \"v1\" [");
    for i in 0..B {
        if i > 0 {
            text.push_str(", ");
        }
        let v = mkvec(i);
        let mut s = String::with_capacity(DIM * 9);
        s.push('[');
        for (d, x) in v.iter().enumerate() {
            if d > 0 {
                s.push(',');
            }
            s.push_str(&format!("{:.5}", x));
        }
        s.push(']');
        text.push_str(&format!(
            "{{*thread:\"t{}\", id:\"m{}\", emb:{}}}",
            i / 25,
            i,
            s
        ));
    }
    text.push(']');
    let text_bytes = text.len();

    // --- 1) PARSE-ONLY (V1 nom), averaged over 5 iterations (pure function) ---
    let iters = 5;
    let t = Instant::now();
    for _ in 0..iters {
        let _ = xytalk_parser::parse(&text).unwrap();
    }
    let parse_us = t.elapsed().as_micros() as f64 / iters as f64;

    // --- 2) EXECUTE (put of the already-parsed PutBatch) ---
    let stmt = xytalk_parser::parse(&text).unwrap();
    let t = Instant::now();
    engine.execute(stmt).unwrap();
    let exec_us = t.elapsed().as_micros() as f64;

    // --- 3) BINARY BULK V3 (no text parse; emb as Value::Vector f32 directly) ---
    let records: Vec<BulkRecord> = (0..B)
        .map(|i| {
            let mut fields = BTreeMap::new();
            fields.insert("id".to_string(), Value::Text(format!("m{i}")));
            fields.insert("emb".to_string(), Value::Vector(mkvec(i)));
            BulkRecord {
                fields,
                gravity_fields: vec![("thread".to_string(), Value::Text(format!("t{}", i / 25)))],
            }
        })
        .collect();
    let t = Instant::now();
    execute_bulk_insert(&engine, "v3", records).unwrap();
    let bulk_us = t.elapsed().as_micros() as f64;

    let v1_total = parse_us + exec_us;
    println!(
        "\n=== INGEST CPU BREAKDOWN · B={B} registros · DIM={DIM} · texto={} KB ===",
        text_bytes / 1024
    );
    println!(
        "parse-only (V1 nom):   {:>9.0} us  ({:>6.2} us/reg)",
        parse_us,
        parse_us / B as f64
    );
    println!(
        "execute (put):         {:>9.0} us  ({:>6.2} us/reg)",
        exec_us,
        exec_us / B as f64
    );
    println!(
        "  V1 total (parse+exec):{:>8.0} us  ({:>6.2} us/reg)",
        v1_total,
        v1_total / B as f64
    );
    println!(
        "bulk binario (V3):     {:>9.0} us  ({:>6.2} us/reg)",
        bulk_us,
        bulk_us / B as f64
    );
    println!(
        "\nfracción del CPU V1 que es PARSE de texto: {:.0}%",
        100.0 * parse_us / v1_total
    );
    println!(
        "speedup V3 binario vs V1 texto: {:.2}x",
        v1_total / bulk_us.max(1.0)
    );
    println!(
        "(si parse% alto y speedup alto → el protocolo de texto ES el cuello → V3/binario lo arregla)\n"
    );
}
