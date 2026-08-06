//! Measure the CEILING of Cauchy–Schwarz early-abort ("lever A") on the real
//! prefix scan: latency with/without pruning over an 8k bucket of VARIED
//! vectors, the prune rate, and the iteration index of the first abort (the
//! visit-order diagnosis). Exactness is gated — the pruned top-k must be
//! bit-identical (score, lid) to the unpruned top-k.
//!   SST=1 cargo test -p xyzdb-engine prune_profile -- --ignored --nocapture
// SPDX-License-Identifier: BUSL-1.1
use crate::engine::Engine;
use std::collections::BinaryHeap;
use std::hint::black_box;
use std::time::{Duration, Instant};
use xyzdb_core::distance::{self, Metric};
use xyzdb_core::key::SpatialKey;
use xyzdb_core::record::read_vector_prefix_raw_norm;

const DIM: usize = 256;
const N: usize = 8000;
const K: usize = 10;
const BLOCK: usize = 32;

/// splitmix64 — deterministic varied output, no deps (`Math.random` is banned
/// in workflow scripts but this is a normal test; still, fixed seed = stable).
fn mix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}
fn comp(s: &mut u64) -> f32 {
    ((mix(s) >> 40) as f32) / ((1u64 << 23) as f32) - 1.0 // ~uniform [-1,1)
}
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
fn profile_cauchy_schwarz_pruning() {
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

    // Corpus + a held-out query. Default: varied random (optimistic prune
    // regime — near-orthogonal, high spread). REAL_EMB=<path to raw
    // little-endian f32, DIM per row> swaps in real embeddings so the prune
    // rate is the one that matters at the product (random over-states it).
    let mut st = 0xDEAD_BEEF_u64;
    const Q: usize = 64; // held-out queries — one number isn't the distribution
    let (corpus, queries): (Vec<Vec<f32>>, Vec<Vec<f32>>) = match std::env::var("REAL_EMB") {
        Ok(path) => {
            let raw = std::fs::read(&path).expect("read REAL_EMB");
            let all: Vec<f32> = raw
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let rows: Vec<Vec<f32>> = all.chunks_exact(DIM).map(|c| c.to_vec()).collect();
            let take = N.min(rows.len().saturating_sub(Q));
            let queries = rows[rows.len() - Q..].to_vec(); // held out from the corpus
            eprintln!(
                "[REAL_EMB: {} rows → {take} corpus + {Q} held-out queries]",
                rows.len()
            );
            (rows[..take].to_vec(), queries)
        }
        Err(_) => {
            let corpus: Vec<Vec<f32>> = (0..N)
                .map(|_| (0..DIM).map(|_| comp(&mut st)).collect())
                .collect();
            let queries: Vec<Vec<f32>> = (0..Q)
                .map(|_| (0..DIM).map(|_| comp(&mut st)).collect())
                .collect();
            (corpus, queries)
        }
    };

    let txt = "word ".repeat(600); // ~3 KB, like the agent/fintech text
    let b = 500;
    for c in (0..corpus.len()).step_by(b) {
        let rows: Vec<String> = (c..(c + b).min(corpus.len()))
            .map(|i| {
                format!(
                    r#"{{*conv:"c1", id:"r{i}", emb:{}, txt:"{txt}"}}"#,
                    vlit(&corpus[i])
                )
            })
            .collect();
        engine
            .execute(
                xytalk_parser::parse(&format!(r#"PUT BATCH IN "mem" [{}]"#, rows.join(",")))
                    .unwrap(),
            )
            .unwrap();
    }
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

    let lobe_id = engine.lobe_registry.read().get("mem").unwrap().id;
    let core = crate::ops::convert_filters(&[xytalk_parser::ast::Filter {
        field: "conv".into(),
        op: xytalk_parser::ast::FilterOp::Eq,
        value: xytalk_parser::ast::Literal::Text("c1".into()),
    }]);
    let gh = crate::ops::scan::detect_gravity_eq(&engine, "mem", &core).expect("gravity-eq");
    let (kmin, kmax) = SpatialKey::prefix_for_gravity(lobe_id, gh);
    let fr = engine.field_registry.read();
    let fd = fr.get_dict(lobe_id);
    let qfid = fd
        .and_then(|d| {
            d.to_names()
                .iter()
                .position(|n| n == "emb")
                .map(|p| p as u16)
        })
        .expect("emb field id");

    // Three exact scorers over the SAME V4 bucket, parametrized by query, so
    // the same code runs for every held-out query:
    //   • unpruned : full similarity_indexed (dot + live nb pass, no abort).
    //   • A        : cosine_pruned, nb computed LIVE (nb2 = None) — dot-pass
    //               pruning only; the nb pass stays.
    //   • C        : cosine_pruned, nb2 = the V4-stored norm — the nb pass
    //               VANISHES, plus the same dot-pass abort.
    let run_unpruned = |q: &[f32]| -> Vec<(u64, u128)> {
        let mut heap: BinaryHeap<super::PrefixCand> = BinaryHeap::with_capacity(K + 1);
        // V5: the scored f32 bytes live in the `vectors` column, not the blob.
        for e in engine
            .turba
            .vectors
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            let Some((lid, fid, fb, _nb2)) = read_vector_prefix_raw_norm(&e.value) else {
                continue;
            };
            if fid != qfid {
                continue;
            }
            let Some(s) = distance::similarity(Metric::Cosine, q, fb) else {
                continue;
            };
            heap.push(super::PrefixCand {
                score: s,
                lid,
                key: e.key.to_vec(),
                column: None,
            });
            if heap.len() > K {
                heap.pop();
            }
        }
        top_sorted(heap)
    };
    // `use_stored_nb` switches A (false ⇒ live) vs C (true ⇒ V4 norm).
    let run_pruned = |q: &[f32], na: f64, suf: &[f64], use_stored_nb: bool| -> Vec<(u64, u128)> {
        let mut heap: BinaryHeap<super::PrefixCand> = BinaryHeap::with_capacity(K + 1);
        // V5: the scored f32 bytes live in the `vectors` column, not the blob.
        for e in engine
            .turba
            .vectors
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            let Some((lid, fid, fb, nb2)) = read_vector_prefix_raw_norm(&e.value) else {
                continue;
            };
            if fid != qfid {
                continue;
            }
            let thr = (heap.len() == K).then(|| heap.peek().unwrap().score);
            let nb2_in = if use_stored_nb { nb2 } else { None };
            let Some(s) = distance::cosine_pruned(q, na, suf, fb, nb2_in, thr, BLOCK) else {
                continue; // aborted (provably worse) or undefined
            };
            heap.push(super::PrefixCand {
                score: s,
                lid,
                key: e.key.to_vec(),
                column: None,
            });
            if heap.len() > K {
                heap.pop();
            }
        }
        top_sorted(heap)
    };
    // Untimed prune RATE for one query (C path) — % of candidates aborted.
    let prune_rate = |q: &[f32], na: f64, suf: &[f64]| -> f64 {
        let mut heap: BinaryHeap<super::PrefixCand> = BinaryHeap::with_capacity(K + 1);
        let (mut scanned, mut aborted) = (0usize, 0usize);
        // V5: the scored f32 bytes live in the `vectors` column, not the blob.
        for e in engine
            .turba
            .vectors
            .range(kmin.as_slice(), kmax.as_slice())
            .unwrap()
        {
            let Some((lid, fid, fb, nb2)) = read_vector_prefix_raw_norm(&e.value) else {
                continue;
            };
            if fid != qfid {
                continue;
            }
            scanned += 1;
            let thr = (heap.len() == K).then(|| heap.peek().unwrap().score);
            match distance::cosine_pruned(q, na, suf, fb, nb2, thr, BLOCK) {
                Some(s) => {
                    heap.push(super::PrefixCand {
                        score: s,
                        lid,
                        key: e.key.to_vec(),
                        column: None,
                    });
                    if heap.len() > K {
                        heap.pop();
                    }
                }
                None => aborted += 1,
            }
        }
        100.0 * aborted as f64 / scanned.max(1) as f64
    };

    // Per query: precompute na+suffix, gate exactness (A & C vs unpruned),
    // record the prune rate, and time all three (median over reps).
    let reps = 5;
    let (mut rates, mut su, mut sa, mut sc) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for q in &queries {
        let na = distance::norm(q);
        let suf = distance::suffix_norm2(q);
        let base = run_unpruned(q);
        assert_eq!(
            base,
            run_pruned(q, na, &suf, false),
            "lever A top-k diverged"
        );
        assert_eq!(
            base,
            run_pruned(q, na, &suf, true),
            "lever C top-k diverged"
        );
        rates.push(prune_rate(q, na, &suf));
        let (mut tu, mut ta, mut tc) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..reps {
            let t = Instant::now();
            black_box(run_unpruned(q));
            tu.push(t.elapsed());
            let t = Instant::now();
            black_box(run_pruned(q, na, &suf, false));
            ta.push(t.elapsed());
            let t = Instant::now();
            black_box(run_pruned(q, na, &suf, true));
            tc.push(t.elapsed());
        }
        su.push(median(tu));
        sa.push(median(ta));
        sc.push(median(tc));
    }
    rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (u, a, c) = (median(su), median(sa), median(sc));
    let rate_med = rates[rates.len() / 2];
    let rate_min = rates[0];
    eprintln!(
        "\n=== Cauchy–Schwarz pruning (A vs C) — corpus={}, {Q} queries, DIM={DIM}, k={K}, block={BLOCK} ===",
        corpus.len()
    );
    eprintln!(
        "  prune rate       : median {rate_med:.1}%  worst {rate_min:.1}%  (data-dependent: the dot abort)"
    );
    eprintln!("  unpruned         (per-query p50) : {u:?}");
    eprintln!(
        "  A live-nb prune  (per-query p50) : {a:?}   speedup {:.3}x  (Δ {:?})",
        u.as_secs_f64() / a.as_secs_f64(),
        u.saturating_sub(a)
    );
    eprintln!(
        "  C stored-nb prune(per-query p50) : {c:?}   speedup {:.3}x  (Δ {:?})",
        u.as_secs_f64() / c.as_secs_f64(),
        u.saturating_sub(c)
    );
    eprintln!(
        "  >>> C over A     : {:.3}x  (Δ {:?})",
        a.as_secs_f64() / c.as_secs_f64(),
        a.saturating_sub(c)
    );
}

/// Drain a bounded top-k heap to the final (score DESC, lid ASC) order as
/// `(score_bits, lid)` pairs — the bit-exact identity the gate compares.
fn top_sorted(heap: BinaryHeap<super::PrefixCand>) -> Vec<(u64, u128)> {
    let mut v: Vec<(f64, u128)> = heap.into_iter().map(|c| (c.score, c.lid.raw())).collect();
    v.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    v.into_iter().map(|(s, l)| (s.to_bits(), l)).collect()
}
