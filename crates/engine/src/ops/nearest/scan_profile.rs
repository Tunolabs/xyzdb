//! TEMP diagnostic: split the FULL NEAREST scan into its cost buckets to
//! find what dominates (the prefix path attacks deserialize, which the
//! measurements suggest is NOT the neck). Run explicitly:
//!   cargo test -p xyzdb-engine scan_profile -- --ignored --nocapture
// SPDX-License-Identifier: BUSL-1.1
use crate::engine::Engine;
use std::collections::BinaryHeap;
use std::hint::black_box;
use std::time::{Duration, Instant};
use xytalk_parser::ast::{PipelineStep, Statement};
use xyzdb_core::distance;
use xyzdb_core::key::SpatialKey;
use xyzdb_core::record::{Record, deserialize_record, hydrate_vector, read_vector_prefix_raw_norm};

fn vlit(v: &[f32]) -> String {
    let p: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
    format!("[{}]", p.join(","))
}
fn median(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    xs[xs.len() / 2]
}

#[test]
#[ignore]
fn profile_full_scan_buckets() {
    // PDIM / PN override the embedding dim and bucket size (defaults 256/8000)
    // so the same profiler runs at the headline dims (e.g. PDIM=1536).
    let dim: usize = std::env::var("PDIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let n: usize = std::env::var("PN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine
        .execute(xytalk_parser::parse(r#"LOBE "mem""#).unwrap())
        .unwrap();
    engine
        .execute(xytalk_parser::parse(r#"GRAVITY BY conv IN "mem""#).unwrap())
        .unwrap();
    engine
        .execute(xytalk_parser::parse(r#"VECTOR emb IN "mem""#).unwrap())
        .unwrap();

    let txt = "word ".repeat(600); // ~3 KB, like the fintech/agent text
    let mut seed_v = vec![0.0f32; dim];
    for (i, x) in seed_v.iter_mut().enumerate() {
        *x = (i as f32).sin();
    }
    let lit = vlit(&seed_v);
    let b = 500;
    for c in (0..n).step_by(b) {
        let rows: Vec<String> = (c..(c + b).min(n))
            .map(|i| format!(r#"{{*conv:"c1", id:"r{i}", emb:{lit}, txt:"{txt}"}}"#))
            .collect();
        engine
            .execute(
                xytalk_parser::parse(&format!(r#"PUT BATCH IN "mem" [{}]"#, rows.join(",")))
                    .unwrap(),
            )
            .unwrap();
    }
    // SST=1 → force a flush+compact so the bucket lives on SST (block
    // decompression on read), the at-scale regime. Unset → memtable.
    let on_sst = std::env::var("SST").is_ok();
    if on_sst {
        engine
            .execute(xytalk_parser::parse("COMPACT").unwrap())
            .unwrap();
    }
    eprintln!(
        "[storage regime: {}]",
        if on_sst {
            "SST (compacted)"
        } else {
            "memtable"
        }
    );

    // Bucket range for conv="c1".
    let lobe_id = engine.lobe_registry.read().get("mem").unwrap().id;
    let core = crate::ops::convert_filters(&[xytalk_parser::ast::Filter {
        field: "conv".into(),
        op: xytalk_parser::ast::FilterOp::Eq,
        value: xytalk_parser::ast::Literal::Text("c1".into()),
    }]);
    let gh = crate::ops::scan::detect_gravity_eq(&engine, "mem", &core).expect("gravity-eq");
    let (kmin, kmax) = SpatialKey::prefix_for_gravity(lobe_id, gh);

    let query = format!(r#"SCAN "mem" WHERE conv="c1" LIMIT 10000 | NEAREST(emb,{lit},10,cosine)"#);
    let (scan, nearest) = match xytalk_parser::parse(&query).unwrap() {
        Statement::Pipeline(s) => match &s[..] {
            [PipelineStep::Scan(sc), PipelineStep::Nearest(ne)] => (sc.clone(), ne.clone()),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };

    let reps = 7;
    let mut count = 0usize;

    // Bucket 1: range iteration + block decompression (touch bytes, NO deserialize).
    let mut t_iter = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let mut nb = 0usize;
        for e in engine
            .turba
            .spatial
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            nb += black_box(e.value.len());
        }
        t_iter.push(t.elapsed());
        count = nb;
    }

    // Bucket 1+2: iterate + full deserialize (no cosine).
    let fr = engine.field_registry.read();
    let fd = fr.get_dict(lobe_id);
    let mut t_deser = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let mut n = 0usize;
        for e in engine
            .turba
            .spatial
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            let r = deserialize_record(&e.value, "mem", fd).unwrap();
            n += black_box(r.fields.len());
        }
        t_deser.push(t.elapsed());
        let _ = n;
    }
    drop(fr);

    // Bucket 1+2+3: full path execute_scan (iter+deser+collect) and +execute_nearest.
    let mut t_scan = Vec::new();
    let mut t_full = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let sr = crate::ops::scan::execute_scan(&engine, scan.clone()).unwrap();
        let recs: Vec<Record> = match sr.query_result {
            crate::engine::QueryResult::Records(r) => r,
            _ => Vec::new(),
        };
        t_scan.push(t.elapsed());
        let t2 = Instant::now();
        let out = super::execute_nearest(black_box(recs), &nearest).unwrap();
        t_full.push(t.elapsed());
        let _ = (black_box(out.len()), t2);
    }

    // The actual fused PREFIX path, in-process (no TCP) — the twin datum.
    let mut t_prefix = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let (out, _truncated) =
            super::execute_scan_nearest(&engine, scan.clone(), &nearest).unwrap();
        t_prefix.push(t.elapsed());
        let _ = black_box(out.len());
    }

    let iter = median(t_iter);
    let deser = median(t_deser);
    let scan = median(t_scan);
    let full = median(t_full);
    let prefix = median(t_prefix);
    eprintln!(
        "\n=== NEAREST scan profile ({n} records, DIM={dim}, ~3KB text/rec, {count} bytes, in-process release) ==="
    );
    eprintln!("  bucket 1  iter + decompress       : {iter:?}");
    eprintln!(
        "  bucket 2  deserialize (1+2 - 1)   : {:?}",
        deser.saturating_sub(iter)
    );
    eprintln!("    (iterate+deserialize total 1+2  : {deser:?})");
    eprintln!("    (execute_scan total             : {scan:?})");
    eprintln!(
        "  bucket 3  cosine + heap (full-scan): {:?}",
        full.saturating_sub(scan)
    );
    eprintln!("  FULL  execute_scan + execute_nearest : {full:?}");
    eprintln!(
        "  PREFIX execute_scan_nearest (fused)  : {prefix:?}   <-- the real prefix path, in-process"
    );
    eprintln!(
        "  >>> in-process prefix/full speedup p50: {:.2}x",
        full.as_secs_f64() / prefix.as_secs_f64()
    );
}

/// Split the FUSED prefix path into (a) range-iterate + value access,
/// (b) +prefix parse, (c) +cosine_pruned score+heap, (d) materialize the k
/// survivors. Run at two dims: (b) is per-RECORD (constant in absolute terms
/// across dim) while (a) is per-BYTE (scales with dim) — so 256d vs 1536d
/// reveals the mechanism, not just a number. A/B/A/B interleaved per rep.
///   PDIM=1536 PK=10 cargo test --release -p xyzdb-engine --lib \
///     profile_fused_internals -- --ignored --nocapture
#[test]
#[ignore]
fn profile_fused_internals() {
    let dim: usize = std::env::var("PDIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let n: usize = std::env::var("PN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);
    let k: usize = std::env::var("PK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let reps = 11; // odd → clean median; A/B/A/B = each rep visits all phases

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    for stmt in [
        r#"LOBE "mem""#,
        r#"GRAVITY BY conv IN "mem""#,
        r#"VECTOR emb IN "mem""#,
    ] {
        engine.execute(xytalk_parser::parse(stmt).unwrap()).unwrap();
    }
    let txt = "word ".repeat(600);
    let mut seed_v = vec![0.0f32; dim];
    for (i, x) in seed_v.iter_mut().enumerate() {
        *x = (i as f32).sin();
    }
    let lit = vlit(&seed_v);
    for c in (0..n).step_by(500) {
        let rows: Vec<String> = (c..(c + 500).min(n))
            .map(|i| format!(r#"{{*conv:"c1", id:"r{i}", emb:{lit}, txt:"{txt}"}}"#))
            .collect();
        engine
            .execute(
                xytalk_parser::parse(&format!(r#"PUT BATCH IN "mem" [{}]"#, rows.join(",")))
                    .unwrap(),
            )
            .unwrap();
    }

    let lobe_id = engine.lobe_registry.read().get("mem").unwrap().id;
    let core = crate::ops::convert_filters(&[xytalk_parser::ast::Filter {
        field: "conv".into(),
        op: xytalk_parser::ast::FilterOp::Eq,
        value: xytalk_parser::ast::Literal::Text("c1".into()),
    }]);
    let gh = crate::ops::scan::detect_gravity_eq(&engine, "mem", &core).expect("gravity-eq");
    let (kmin, kmax) = SpatialKey::prefix_for_gravity(lobe_id, gh);
    let lobe_name = engine.lobe_name_for_id(lobe_id);
    let fr = engine.field_registry.read();
    let fd = fr.get_dict(lobe_id);
    let qfid: u16 = fd
        .and_then(|d| {
            d.to_names()
                .iter()
                .position(|nm| nm == "emb")
                .map(|p| p as u16)
        })
        .expect("emb field id");
    // query == corpus → all scores equal → no pruning → (c) is the honest
    // worst case (full score of every candidate, never an early abort).
    let query: Vec<f32> = seed_v.clone();
    let na = distance::norm(&query);
    let suf = distance::suffix_norm2(&query);

    // (a) range the vectors column + touch each value.
    let phase_a = || {
        let mut bytes = 0usize;
        for e in engine
            .turba
            .vectors
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            bytes += e.value.len();
        }
        bytes
    };
    // (a)+(b) add the per-entry prefix parse + field filter (no score).
    let phase_ab = || {
        let mut acc = 0usize;
        for e in engine
            .turba
            .vectors
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            let Some((lid, fid, fb, nb)) = read_vector_prefix_raw_norm(&e.value) else {
                continue;
            };
            if fid != qfid {
                continue;
            }
            acc += fb.len() ^ (lid.raw() as usize) ^ nb.map_or(0, |x| x.to_bits() as usize);
        }
        acc
    };
    // (a)+(b)+(c) add cosine_pruned + the bounded top-k heap; returns the
    // sorted survivors (spatial key + V5 column bytes) for phase (d).
    let phase_abc = || -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut heap: BinaryHeap<super::PrefixCand> = BinaryHeap::with_capacity(k + 1);
        for e in engine
            .turba
            .vectors
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            let Some((lid, fid, fb, nb)) = read_vector_prefix_raw_norm(&e.value) else {
                continue;
            };
            if fid != qfid {
                continue;
            }
            let thr = (heap.len() == k)
                .then(|| heap.peek().map(|c| c.score))
                .flatten();
            let Some(s) =
                distance::cosine_pruned(&query, na, &suf, fb, nb, thr, super::PRUNE_BLOCK)
            else {
                continue;
            };
            heap.push(super::PrefixCand {
                score: s,
                lid,
                key: e.key.to_vec(),
                column: Some(e.value.to_vec()),
            });
            if heap.len() > k {
                heap.pop();
            }
        }
        let mut cands = heap.into_vec();
        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.lid.cmp(&b.lid))
        });
        cands
            .into_iter()
            .map(|c| (c.key, c.column.unwrap()))
            .collect()
    };
    // (d) materialize the survivors exactly as the fused tail does: a point-get
    // per survivor (random access into the spatial keyspace) + deserialize +
    // hydrate the vector from the column bytes already held.
    let materialize = |survivors: &[(Vec<u8>, Vec<u8>)]| {
        let mut fields = 0usize;
        for (key, col) in survivors {
            let blob = engine.turba.spatial.get(key).unwrap().unwrap();
            let mut rec = deserialize_record(&blob, &lobe_name, fd).unwrap();
            if let Some(dict) = fd {
                hydrate_vector(&mut rec, col, dict);
            }
            fields += rec.fields.len();
        }
        fields
    };

    let survivors = phase_abc(); // computed once → input to (d)
    let (mut ta, mut tab, mut tabc, mut td) = (vec![], vec![], vec![], vec![]);
    for _ in 0..reps {
        let t = Instant::now();
        black_box(phase_a());
        ta.push(t.elapsed());
        let t = Instant::now();
        black_box(phase_ab());
        tab.push(t.elapsed());
        let t = Instant::now();
        black_box(phase_abc());
        tabc.push(t.elapsed());
        let t = Instant::now();
        black_box(materialize(&survivors));
        td.push(t.elapsed());
    }
    let (a, ab, abc, d) = (median(ta), median(tab), median(tabc), median(td));
    let per = |t: Duration, c: usize| t.as_secs_f64() * 1e9 / c as f64; // ns per item
    eprintln!("\n=== fused internals (dim={dim}, n={n}, k={k}, reps={reps}, memtable) ===");
    eprintln!(
        "  (a) iterate + value access : {a:?}   ({:.0} ns/entry)",
        per(a, n)
    );
    eprintln!(
        "  (b) prefix parse           : {:?}   ({:.0} ns/entry, per-RECORD)",
        ab.saturating_sub(a),
        per(ab.saturating_sub(a), n)
    );
    eprintln!(
        "  (c) cosine_pruned + heap   : {:?}   ({:.0} ns/entry)",
        abc.saturating_sub(ab),
        per(abc.saturating_sub(ab), n)
    );
    eprintln!(
        "  (d) materialize {k:>3} surv. : {d:?}   ({:.0} ns/get, RANDOM)",
        per(d, k)
    );
    eprintln!("  sum (a+b+c)+(d)            : {:?}", abc + d);
}
