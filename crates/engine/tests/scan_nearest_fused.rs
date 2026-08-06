//! Gate tests for the fused `[Scan, Nearest]` V3 prefix fast path.
//!
//! The fused path reads only the hoisted V3 vector prefix per record in the
//! gravity bucket, ranks the top-k, and fully deserializes only the survivors.
//! These gates are the equivalence contract:
//!
//! * Gate A — recall@10 = 1.0: every planted target is its own exact nearest.
//! * Gate B1 — the fused prefix path is bit-identical (same lid sequence,
//!   including tie/near-tie order) to the forced full path on a V3 bucket.
//! * Gate B2 — when no vector is declared (records stay V1), the fused entry
//!   still routes through the full path and matches it unchanged.

// SPDX-License-Identifier: BUSL-1.1
use std::sync::atomic::Ordering::Relaxed;
use xytalk_parser::ast::{PipelineStep, Statement};
use xyzdb_core::error::XyzError;
use xyzdb_core::lid::LID;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};
use xyzdb_engine::ops::nearest::FORCE_NEAREST_STRATEGY_B;

/// Embedding width. ≥ `VECTOR_F32_MIN_DIMS` (64) so the executor packs the PUT
/// list literal into a `Value::Vector` and the record is stored V3.
const DIM: usize = 64;

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

/// Records (lid + ordered) out of a query result.
fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(recs) => recs,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected Records, got {other:?}"),
    }
}

/// The ordered `lid` sequence — the equivalence unit for the bit-exact gate.
fn lids(qr: QueryResult) -> Vec<LID> {
    records(qr).into_iter().map(|r| r.lid).collect()
}

/// The ordered `id` text fields.
fn ids(qr: QueryResult) -> Vec<String> {
    records(qr)
        .into_iter()
        .map(|r| match r.fields.get("id") {
            Some(Value::Text(t)) => t.clone(),
            other => panic!("record without id: {other:?}"),
        })
        .collect()
}

/// Format a float vector as a xyTalk list literal `[f, f, ...]`.
fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
    format!("[{}]", parts.join(", "))
}

/// Run the SAME parsed `SCAN | NEAREST` through the FORCED full path:
/// `execute_scan` → `execute_nearest`, the exact fallback the fused fn calls.
fn full_path(engine: &Engine, query: &str) -> Vec<xyzdb_core::record::Record> {
    let stmt = xytalk_parser::parse(query).unwrap_or_else(|e| panic!("parse {query:?}: {e:?}"));
    let steps = match stmt {
        Statement::Pipeline(steps) => steps,
        other => panic!("expected a pipeline, got {other:?}"),
    };
    let (scan, nearest) = match &steps[..] {
        [PipelineStep::Scan(s), PipelineStep::Nearest(n)] => (s.clone(), n.clone()),
        other => panic!("expected [Scan, Nearest], got {other:?}"),
    };
    let scan_result =
        xyzdb_engine::ops::scan::execute_scan(engine, scan).expect("forced full-path scan");
    let recs = records(scan_result.query_result);
    xyzdb_engine::ops::nearest::execute_nearest(recs, &nearest).expect("forced full-path nearest")
}

// ─── Gate A: recall@10 = 1.0 ────────────────────────────────────────────────

#[test]
fn gate_a_recall_at_10_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // Plant N target vectors in one gravity bucket. Each target is a distinct
    // one-hot-ish direction so the exact nearest of `target + tiny noise` is the
    // target itself.
    let n: usize = 25;
    for t in 0..n {
        let mut v = vec![0.0f32; DIM];
        v[t % DIM] = 1.0;
        v[(t * 7 + 3) % DIM] += 0.5; // a second active coord, still target-unique
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"t{t}", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }

    for t in 0..n {
        let mut v = vec![0.0f32; DIM];
        v[t % DIM] = 1.0;
        v[(t * 7 + 3) % DIM] += 0.5;
        v[(t * 13 + 1) % DIM] += 0.001; // tiny noise — target stays the nearest
        let q = format!(
            r#"SCAN "mem" WHERE conv="c1" | NEAREST(emb, {}, 10, cosine)"#,
            vec_literal(&v)
        );
        let top = ids(exec(&engine, &q));
        assert!(
            top.iter().any(|id| id == &format!("t{t}")),
            "recall@10: target t{t} not in top-10 {top:?}"
        );
    }
}

// ─── Gate B1: prefix path vs forced full path on a V3 bucket ─────────────────

#[test]
fn gate_b1_prefix_matches_full_path_with_ties() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // A spread of vectors PLUS several with the exact same vector, so distinct
    // records tie on score and the (score DESC, lid ASC) tiebreak is exercised.
    let mut tied = vec![0.0f32; DIM];
    tied[0] = 1.0;
    tied[1] = 1.0;
    for i in 0..6 {
        // 6 records sharing the identical `tied` vector → equal scores.
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"tie{i}", emb:{}}} IN "mem""#,
                vec_literal(&tied)
            ),
        );
    }
    // Plus distinct neighbours at varied distances, some near-tied.
    for i in 0..20 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[1] = 1.0;
        v[(i % (DIM - 2)) + 2] = 0.05 + (i as f32) * 0.01; // near-tied cluster
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"n{i}", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    // A record without the emb field (skipped, same as full path).
    exec(
        &engine,
        r#"PUT {*conv:"c1", id:"noemb", note:"x"} IN "mem""#,
    );
    // A record in a different bucket — must never surface.
    let mut other = vec![0.0f32; DIM];
    other[0] = 1.0;
    other[1] = 1.0;
    exec(
        &engine,
        &format!(
            r#"PUT {{*conv:"c2", id:"intruder", emb:{}}} IN "mem""#,
            vec_literal(&other)
        ),
    );

    let mut q = vec![0.0f32; DIM];
    q[0] = 1.0;
    q[1] = 1.0;
    for metric in ["cosine", "dot", "l2"] {
        for k in [1u64, 3, 8, 50] {
            let query = format!(
                r#"SCAN "mem" WHERE conv="c1" | NEAREST(emb, {}, {k}, {metric})"#,
                vec_literal(&q)
            );
            let fused = lids(exec(&engine, &query));
            let full: Vec<LID> = full_path(&engine, &query)
                .into_iter()
                .map(|r| r.lid)
                .collect();
            assert_eq!(
                fused, full,
                "fused prefix path != full path for metric={metric} k={k}"
            );
            // The other-bucket record is never present.
            let fused_ids = ids(exec(&engine, &query));
            assert!(
                !fused_ids.iter().any(|id| id == "intruder"),
                "cross-bucket record leaked: {fused_ids:?}"
            );
        }
    }
}

// ─── Gate B2: fallback unchanged when no vector is declared (V1 records) ──────

#[test]
fn gate_b2_fallback_matches_full_path_without_vector_decl() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    // No VECTOR declaration: records stay V1, the prefix path cannot apply, the
    // fused entry MUST fall back to scan→execute_nearest unchanged.
    exec(&engine, r#"LOBE "fin""#);
    for i in 0..15 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[(i % (DIM - 1)) + 1] = (i as f32) * 0.1;
        exec(
            &engine,
            &format!(
                r#"PUT {{*acct:"a1", id:"r{i}", emb:{}}} IN "fin""#,
                vec_literal(&v)
            ),
        );
    }

    let mut q = vec![0.0f32; DIM];
    q[0] = 1.0;
    let query = format!(
        r#"SCAN "fin" WHERE acct="a1" | NEAREST(emb, {}, 5, cosine)"#,
        vec_literal(&q)
    );
    let fused = lids(exec(&engine, &query));
    let full: Vec<LID> = full_path(&engine, &query)
        .into_iter()
        .map(|r| r.lid)
        .collect();
    assert_eq!(fused, full, "fused fallback != full path on V1 lobe");
}

// ─── Bonus: fallback when NEAREST field != declared vector field ─────────────

#[test]
fn fallback_when_field_is_not_the_declared_vector() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);
    for i in 0..10 {
        let mut emb = vec![0.0f32; DIM];
        emb[0] = 1.0;
        emb[(i % (DIM - 1)) + 1] = (i as f32) * 0.1;
        let mut other = vec![0.0f32; DIM];
        other[1] = 1.0;
        other[(i % (DIM - 2)) + 2] = (i as f32) * 0.2;
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"r{i}", emb:{}, alt:{}}} IN "mem""#,
                vec_literal(&emb),
                vec_literal(&other)
            ),
        );
    }

    // Query the NON-declared field `alt`: must fall back and still match.
    let mut q = vec![0.0f32; DIM];
    q[1] = 1.0;
    let query = format!(
        r#"SCAN "mem" WHERE conv="c1" | NEAREST(alt, {}, 4, cosine)"#,
        vec_literal(&q)
    );
    let fused = lids(exec(&engine, &query));
    let full: Vec<LID> = full_path(&engine, &query)
        .into_iter()
        .map(|r| r.lid)
        .collect();
    assert_eq!(fused, full, "non-declared-field query must match full path");
}

// ─── M2.2: --nearest-budget-ms airbag ───────────────────────────────────────

/// A large single gravity bucket scanned with a 1ms budget must abort with
/// [`XyzError::NearestBudgetExceeded`] at a stride boundary — the explicit,
/// actionable failure that replaces a silent multi-second hang once NEAREST is
/// decoupled from the SCAN cap. The default (generous) budget must NOT fire on
/// the same bucket, and a 0 budget disables the airbag entirely.
#[test]
fn nearest_budget_aborts_runaway_bucket_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"GRAVITY BY conv IN "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // One bucket, many identical vectors — enough that the fused scan runs well
    // past 1ms and crosses several `BUDGET_CHECK_STRIDE` (1024) boundaries above
    // the budget, so the abort is robust in both debug and release.
    let dim = 256usize;
    let n = 8000usize;
    let mut v = vec![0.0f32; dim];
    v[0] = 1.0;
    let lit = vec_literal(&v);
    let mut i = 0;
    while i < n {
        let rows: Vec<String> = (i..(i + 500).min(n))
            .map(|j| format!(r#"{{*conv:"c1", id:"r{j}", emb:{lit}}}"#))
            .collect();
        exec(
            &engine,
            &format!(r#"PUT BATCH IN "mem" [{}]"#, rows.join(",")),
        );
        i += 500;
    }

    // No LIMIT: the fused path scans the whole bucket (it ignores the SCAN cap),
    // which is exactly the runaway the airbag guards.
    let query = format!(r#"SCAN "mem" WHERE conv="c1" | NEAREST(emb, {lit}, 10, cosine)"#);

    // Default budget (3000ms, calibrated) is generous → an 8k bucket completes.
    assert!(
        engine.run(&query).is_ok(),
        "default budget must not fire on an 8k bucket"
    );

    // 1ms budget is far below the scan time → abort with the actionable error.
    engine.set_nearest_budget_ms(1);
    match engine.run(&query) {
        Err(XyzError::NearestBudgetExceeded { scanned, budget_ms }) => {
            assert_eq!(budget_ms, 1, "echoes the configured budget");
            assert!(
                scanned >= 1024,
                "fired before the first stride check: {scanned}"
            );
        }
        other => panic!("expected NearestBudgetExceeded, got {other:?}"),
    }

    // 0 disables the airbag → the scan completes again.
    engine.set_nearest_budget_ms(0);
    assert!(
        engine.run(&query).is_ok(),
        "budget 0 must disable the airbag"
    );
}

// ─── M2.1: NEAREST decoupled from the SCAN cap (recall cliff closed) ─────────

/// The recall cliff this whole milestone exists to close. An UNFUSED NEAREST
/// (the lobe declares NO `VECTOR`, so `try_prefix` returns None and the full
/// path runs) over a gravity bucket bigger than `SCAN_LIMIT_DEFAULT`: the true
/// top-10 are planted at the END of the bucket (highest lids), past the cap.
///
/// Before M2.1 the feeding SCAN caps at 1000, never scans the planted
/// neighbours, and returns recall 0. After M2.1 the NEAREST-feeding scan is
/// uncapped (guarded by the budget) and returns recall 1.0. This test asserts
/// the post-fix contract — it is RED before the fix, GREEN after.
#[test]
fn nearest_unfused_recall_is_one_past_the_scan_cap() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"GRAVITY BY conv IN "mem""#);
    // No `VECTOR emb`: records stay un-hoisted → NEAREST runs the unfused full
    // path, whose feeding SCAN is the capped one.

    let n = 12_000usize; // > SCAN_LIMIT_DEFAULT (1000)
    let near_count = 10usize; // the true top-10, planted LAST (highest lids)
    let mut far = vec![0.0f32; DIM];
    far[0] = 1.0; // filler, orthogonal to the query
    let mut near = vec![0.0f32; DIM];
    near[1] = 1.0; // aligned with the query
    let far_lit = vec_literal(&far);
    let near_lit = vec_literal(&near);
    let mut i = 0;
    while i < n {
        let rows: Vec<String> = (i..(i + 500).min(n))
            .map(|j| {
                let emb = if j >= n - near_count {
                    &near_lit
                } else {
                    &far_lit
                };
                format!(r#"{{*conv:"c1", id:"r{j}", emb:{emb}}}"#)
            })
            .collect();
        exec(
            &engine,
            &format!(r#"PUT BATCH IN "mem" [{}]"#, rows.join(",")),
        );
        i += 500;
    }

    let query = format!(r#"SCAN "mem" WHERE conv="c1" | NEAREST(emb, {near_lit}, 10, cosine)"#);
    let got = ids(exec(&engine, &query));
    let expected: std::collections::HashSet<String> =
        ((n - near_count)..n).map(|j| format!("r{j}")).collect();
    let hits = got.iter().filter(|id| expected.contains(*id)).count();
    assert_eq!(
        hits, near_count,
        "recall@10 must be 1.0 over a {n}-record bucket; got {hits}/{near_count} \
         (the SCAN cap cliff). returned={got:?}"
    );
}

// ─── M2.3: fused residual-filter NEAREST via hydrate-until-k ─────────────────

/// Equivalence gate (NOT RED→GREEN — the residual case is already recall-1.0 via
/// M2.1). The fused hydrate-until-k path must return the bit-identical top-k to
/// the forced full path when a NEAREST carries a non-gravity residual filter.
///
/// The trap that guards the invariants by construction: the records NEAREST the
/// query are `topic="y"` (excluded by the residual); the true winners are
/// `topic="x"` records further out. A "top-k scored" cut, or a score-prune that
/// aborts on the k-th best score, would wrongly surface the excluded `y` records.
/// hydrate-until-k must descend past them and keep only `x` passers until k.
#[test]
fn gate_m23_fused_residual_matches_full_path() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"GRAVITY BY conv IN "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    let mut q = vec![0.0f32; DIM];
    q[0] = 1.0;
    // 6 topic="y" almost exactly on the query axis (highest cosine) — EXCLUDED.
    for i in 0..6 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[1] = (i as f32) * 0.001;
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"y{i}", topic:"y", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    // 20 topic="x" at varied distances — the pool the top-k is actually drawn from.
    for i in 0..20 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[(i % (DIM - 1)) + 1] = 0.1 + (i as f32) * 0.02;
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"x{i}", topic:"x", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    // A topic="x" record in a DIFFERENT gravity bucket — must never surface.
    exec(
        &engine,
        &format!(
            r#"PUT {{*conv:"c2", id:"intruder", topic:"x", emb:{}}} IN "mem""#,
            vec_literal(&q)
        ),
    );

    for k in [1u64, 5, 10, 50] {
        let query = format!(
            r#"SCAN "mem" WHERE conv="c1" AND topic="x" | NEAREST(emb, {}, {k}, cosine)"#,
            vec_literal(&q)
        );
        let fused = lids(exec(&engine, &query));
        let full: Vec<LID> = full_path(&engine, &query)
            .into_iter()
            .map(|r| r.lid)
            .collect();
        assert_eq!(fused, full, "fused residual != full path for k={k}");
        // Only topic="x", same-bucket records may appear (no y, no cross-bucket).
        let got = ids(exec(&engine, &query));
        assert!(
            got.iter().all(|id| id.starts_with('x')),
            "residual leaked a non-x / cross-bucket record for k={k}: {got:?}"
        );
    }
}

// ─── M2.3 airbag contract: latency wall, never a recall/Err wall ─────────────
//
// A selective-residual hydration bug found in a downstream integration: a selective residual (fewer
// than k rows pass) forces hydrating the whole bucket in score order; when that
// exceeded `--nearest-budget-ms` the engine returned
// `Err(NearestBudgetExceeded)`, turning a legitimate small answer into a failure.
// The fix: the bounded hydration tail DEGRADES — it returns the score-ordered
// prefix found so far, flagged truncated via `PaginatedRecords { has_more: true,
// cursor: None }` — instead of failing. (The unbounded scoring scan still
// hard-fails; that is the airbag's real job, see
// `nearest_budget_aborts_runaway_bucket_scan`.)
//
// These gates run in the POST-RESTART regime (COMPACT + clean reopen): on disk
// the hydration point-gets are real I/O, so the trap actually bites — it is
// invisible while the bucket still sits in the write memtable.

/// Load `n` rows into one gravity bucket, COMPACT, and reopen so every hydration
/// point-get hits disk. The first `rare` rows carry `topic:"rare"` on the axis-1
/// embedding; the rest carry `topic:"common"` on the axis-0 embedding. Each row
/// also carries a ~4 KB `filler` so a hydration point-get (full-blob read +
/// deserialize) is materially heavier than a scoring column read (~256 B) — the
/// asymmetry that lets the budget bite in hydration, not scoring.
///
/// `n` is kept below `BUDGET_CHECK_STRIDE` (1024) on purpose: the airbag only
/// checks the clock at multiples of the stride, so with fewer than 1024
/// candidates the scoring pass NEVER reaches a check — the first (and only) check
/// falls in the hydration pass, exactly the phase under test. This makes the trip
/// deterministic in phase (a scoring-phase `Err` is impossible by construction),
/// while the wall-clock only decides truncated-vs-complete, both of which are OK.
fn load_selective_bucket_on_disk(dir: &std::path::Path, n: usize, rare: usize) -> Engine {
    assert!(n < 1024, "keep n below the stride so scoring never trips");
    let engine = Engine::open(dir).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"GRAVITY BY conv IN "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    let mut common = vec![0.0f32; DIM];
    common[0] = 1.0; // axis-0
    let common_lit = vec_literal(&common);
    let mut rare_v = vec![0.0f32; DIM];
    rare_v[1] = 1.0; // axis-1
    let rare_lit = vec_literal(&rare_v);
    let filler = "x".repeat(4096); // heavy blob → costly hydration point-get

    let mut i = 0usize;
    while i < n {
        let rows: Vec<String> = (i..(i + 500).min(n))
            .map(|j| {
                if j < rare {
                    format!(
                        r#"{{*conv:"c1", id:"rare{j}", topic:"rare", filler:"{filler}", emb:{rare_lit}}}"#
                    )
                } else {
                    format!(
                        r#"{{*conv:"c1", id:"cmn{j}", topic:"common", filler:"{filler}", emb:{common_lit}}}"#
                    )
                }
            })
            .collect();
        exec(
            &engine,
            &format!(r#"PUT BATCH IN "mem" [{}]"#, rows.join(",")),
        );
        i += 500;
    }
    engine.run("COMPACT").expect("compact to disk");
    drop(engine);
    Engine::open(dir).unwrap() // clean reopen: cold caches, data on disk
}

/// A query aligned with axis `a` (the direction those rows sit on).
fn axis_query(a: usize) -> String {
    let mut q = vec![0.0f32; DIM];
    q[a] = 1.0;
    vec_literal(&q)
}

/// Outcome (b): fewer than k rows pass, and the budget is ample. The whole bucket
/// is hydrated within budget, so the answer is COMPLETE — exactly the passers,
/// `QueryResult::Records` (not truncated). This is the case that used to `Err`
/// once the bucket grew large enough to blow the budget; here the budget is
/// disabled so the outcome is deterministic regardless of machine speed.
#[test]
fn m23_complete_but_short_is_records_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = load_selective_bucket_on_disk(dir.path(), 800, 2);
    engine.set_nearest_budget_ms(0); // airbag disabled → always completes

    let query = format!(
        r#"SCAN "mem" WHERE conv="c1" AND topic="rare" | NEAREST(emb, {}, 10, cosine)"#,
        axis_query(1)
    );
    match engine
        .run(&query)
        .expect("selective residual must not error")
    {
        QueryResult::Records(recs) => {
            let ids: Vec<String> = recs
                .iter()
                .filter_map(|r| match r.fields.get("id") {
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(ids.len(), 2, "exactly the two rare passers: {ids:?}");
            assert!(
                ids.iter().all(|id| id.starts_with("rare")),
                "only rare rows may pass: {ids:?}"
            );
        }
        QueryResult::PaginatedRecords { .. } => {
            panic!("complete-but-short must be Records, not a truncated result")
        }
        other => panic!("expected Records, got {other:?}"),
    }
}

/// Outcome (c): fewer than k rows pass AND the budget bites mid-hydration. The
/// query must NEVER `Err` (the regression); it returns a prefix-correct partial.
/// Robust to machine speed: on a slow disk the budget truncates (assert the
/// truncation contract), on a very fast one it completes (assert the exact
/// passers) — but it is `Ok` either way, which is the whole point of the fix.
#[test]
fn m23_budget_cut_degrades_to_prefix_not_err() {
    let dir = tempfile::tempdir().unwrap();
    // n < 1024 so the scoring pass never reaches a stride check → a scoring-phase
    // Err is impossible; the only budget check lands in the hydration pass. The
    // two rare rows sit on axis-1 and are hydrated FIRST (the query is axis-1), so
    // a truncated result still carries them — a non-trivial prefix, not an empty one.
    let mut engine = load_selective_bucket_on_disk(dir.path(), 1000, 2);

    let query = format!(
        r#"SCAN "mem" WHERE conv="c1" AND topic="rare" | NEAREST(emb, {}, 10, cosine)"#,
        axis_query(1)
    );

    // The complete, exact answer (score DESC, lid ASC) via the forced full path —
    // the two rare rows. A truncated partial must be a PREFIX of this.
    let full_ids: Vec<String> = full_path(&engine, &query)
        .into_iter()
        .filter_map(|r| match r.fields.get("id") {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();

    engine.set_nearest_budget_ms(1); // 1ms: far below the on-disk hydration of ~1000 heavy blobs
    let got = engine
        .run(&query)
        .expect("a selective residual must degrade, never Err on budget");
    match got {
        QueryResult::PaginatedRecords {
            records,
            cursor,
            has_more,
            budget_stop,
        } => {
            assert!(has_more, "a budget-truncated NEAREST sets has_more");
            assert!(
                cursor.is_none(),
                "NEAREST truncation carries NO cursor (not resumable)"
            );
            // M2.3 flag (fill-path, deterministic here): a budget stop carries the
            // counters, turning "there may be more" into "examined E of C, found F".
            let bs = budget_stop.expect("budget-truncated NEAREST must carry budget_stop");
            assert_eq!(bs.found, records.len(), "found == records returned");
            assert!(bs.candidates >= bs.found, "candidates >= found: {bs:?}");
            assert!(
                bs.examined >= bs.found && bs.examined <= bs.candidates,
                "examined in [found, candidates]: {bs:?}"
            );
            let ids: Vec<String> = records
                .iter()
                .filter_map(|r| match r.fields.get("id") {
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                ids.iter().all(|id| id.starts_with("rare")),
                "truncated partial may only contain passers: {ids:?}"
            );
            // Prefix-correctness: whatever came back is the highest-scoring
            // prefix of the true answer, in order — not an arbitrary sample.
            assert_eq!(
                ids,
                full_ids[..ids.len()],
                "truncated partial must be a score-ordered PREFIX of the true answer"
            );
        }
        QueryResult::Records(recs) => {
            // Fast machine finished within 1ms → complete, exact, not truncated.
            let ids: Vec<String> = recs
                .iter()
                .filter_map(|r| match r.fields.get("id") {
                    Some(Value::Text(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                ids, full_ids,
                "a complete result must equal the full answer"
            );
        }
        other => panic!("expected Records or PaginatedRecords, got {other:?}"),
    }
}

/// No-regression of the common (abundant) case: many rows pass the residual, so
/// the `out.len() == k` success-break fires early and the path pays nothing for
/// the budget machinery. It must return a COMPLETE `QueryResult::Records` (never
/// a truncated result) and match the forced full path bit-for-bit.
#[test]
fn m23_abundant_residual_stays_complete_records() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"GRAVITY BY conv IN "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // 30 topic="hit" rows at varied distances (>> k=10) plus 5 excluded rows.
    for i in 0..30 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[(i % (DIM - 1)) + 1] = 0.1 + (i as f32) * 0.02;
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"hit{i}", topic:"hit", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    for i in 0..5 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[1] = (i as f32) * 0.001; // nearest to the query, but EXCLUDED
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"skip{i}", topic:"skip", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    engine.run("COMPACT").expect("compact to disk");
    drop(engine);
    let engine = Engine::open(dir.path()).unwrap();

    let query = format!(
        r#"SCAN "mem" WHERE conv="c1" AND topic="hit" | NEAREST(emb, {}, 10, cosine)"#,
        axis_query(0)
    );
    let qr = engine.run(&query).expect("abundant residual");
    assert!(
        matches!(qr, QueryResult::Records(_)),
        "abundant residual must be a complete Records result, got {qr:?}"
    );
    let fused = lids(qr);
    let full: Vec<LID> = full_path(&engine, &query)
        .into_iter()
        .map(|r| r.lid)
        .collect();
    assert_eq!(fused.len(), 10, "k=10 winners");
    assert_eq!(
        fused, full,
        "abundant fused residual must match the full path"
    );
}

// ─── A/B step 1: the accumulator refactor must not move a single row ─────────

/// The fused path's accumulator became a bounded top-k heap (so a later
/// key-ordered pass can use it). A heap pops in the OPPOSITE order to the prefix
/// it replaced, so the output order is re-established by an explicit final sort.
/// If that sort is wrong, or missing, ties resolve differently and the k-th row
/// silently becomes a different record.
///
/// Ties are the whole point and they are not hypothetical: identical digests give
/// identical vectors and therefore EXACTLY equal scores. Here more records tie than
/// fit in k, so the tie-break decides the CUT itself, not just the ordering — the
/// sharpest form of the check.
///
/// Asserted ROW FOR ROW against the unfused full path, which did not change. A
/// "same set" assertion would pass while the order silently inverted, and that is
/// precisely the mistake this gate exists to catch.
#[test]
fn ab_step1_accumulator_keeps_the_fused_path_bit_identical_under_ties() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // One vector shared by MANY records → exactly equal scores against the query.
    let mut tied = vec![0.0f32; DIM];
    tied[0] = 1.0;
    // 9 tied records that pass the residual, k = 4 → the tie-break picks the cut.
    for i in 0..9 {
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"tie{i}", keep:"yes", emb:{}}} IN "mem""#,
                vec_literal(&tied)
            ),
        );
    }
    // Same tied vector but FAILING the residual: they must never appear, and they
    // force the hydrate-until-k path (a predicate on a non-gravity field).
    for i in 0..6 {
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"drop{i}", keep:"no", emb:{}}} IN "mem""#,
                vec_literal(&tied)
            ),
        );
    }
    // Distinct, farther vectors so the top-k is not the whole bucket.
    for i in 0..20 {
        let mut v = vec![0.0f32; DIM];
        v[(i % (DIM - 1)) + 1] = 1.0;
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"far{i}", keep:"yes", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }

    let q = format!(
        r#"SCAN "mem" WHERE conv="c1" AND keep="yes" | NEAREST(emb, {}, 4, cosine)"#,
        vec_literal(&tied)
    );

    let fused = lids(exec(&engine, &q));
    let full: Vec<LID> = full_path(&engine, &q).into_iter().map(|r| r.lid).collect();

    assert_eq!(fused.len(), 4, "top-4 expected, got {}", fused.len());
    assert_eq!(
        fused, full,
        "fused and full-path top-k diverged ROW FOR ROW under ties — the \
         accumulator's final ordering is not the full path's (score desc, lid asc)"
    );

    // And the residual still holds: nothing marked keep="no" may appear.
    let got_ids = ids(exec(&engine, &q));
    assert!(
        got_ids.iter().all(|i| !i.starts_with("drop")),
        "a record failing the residual leaked into the top-k: {got_ids:?}"
    );
}

// ─── A/B step 2: B walks in key order and must EXHAUST the range ─────────────

/// B feeds the same candidates in KEY order instead of score order. What step 2
/// can break is not the tie-break — it is the EARLY EXIT: under B the survivors
/// emerge in key order, so the first k passers are not the k best.
///
/// The step-1 corpus cannot catch that, and the reason is worth stating: there
/// every vector was identical, so every score tied, so stopping early would return
/// k tied passers — a valid top-k in all but `lid`, indistinguishable from the
/// right answer. The gate would pass with B broken.
///
/// So this corpus has the INVERSE property: score grows strictly along key order,
/// which puts the best passers at the TAIL. Any early stop therefore discards a
/// winner visibly. `id` is assigned so key order tracks insertion order, and the
/// query vector matches the LAST inserted record.
#[test]
fn ab_step2_key_order_must_exhaust_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // 40 records whose similarity to the query INCREASES with insertion order:
    // coordinate 0 grows, so the last ones are the nearest. Half fail the residual
    // so the hydrate-until-k path is exercised and the winners are interleaved.
    let n = 40usize;
    for i in 0..n {
        let mut v = vec![0.0f32; DIM];
        v[0] = (i as f32 + 1.0) / n as f32; // strictly increasing along the key
        v[1] = 1.0 - v[0];
        let keep = if i % 2 == 0 { "yes" } else { "no" };
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"r{i:03}", keep:"{keep}", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    // The query IS the direction the tail points at, so the best passers are last.
    let mut q = vec![0.0f32; DIM];
    q[0] = 1.0;
    let query = format!(
        r#"SCAN "mem" WHERE conv="c1" AND keep="yes" | NEAREST(emb, {}, 5, cosine)"#,
        vec_literal(&q)
    );

    // A (score order, early exit allowed) — the reference.
    FORCE_NEAREST_STRATEGY_B.store(false, Relaxed);
    let a_lids = lids(exec(&engine, &query));
    let a_ids = ids(exec(&engine, &query));

    // B (key order, must exhaust).
    FORCE_NEAREST_STRATEGY_B.store(true, Relaxed);
    let b_lids = lids(exec(&engine, &query));
    FORCE_NEAREST_STRATEGY_B.store(false, Relaxed);

    assert_eq!(a_lids.len(), 5, "top-5 expected");
    assert_eq!(
        b_lids, a_lids,
        "B diverged from A ROW FOR ROW — if B stopped at k it kept the first \
         passers in KEY order, which on this corpus are the WORST ones"
    );
    // Sanity on the corpus itself: the winners really are at the tail, so an early
    // stop would have been visible. Without this, the gate could be green because
    // the corpus was too easy rather than because B is right.
    assert!(
        a_ids.iter().all(|id| {
            id.strip_prefix('r')
                .and_then(|n| n.parse::<usize>().ok())
                .is_some_and(|n| n >= n_tail_threshold())
        }),
        "corpus is not adversarial: winners must sit at the tail of key order, got {a_ids:?}"
    );
}

/// Winners must live in the last quarter of key order for the step-2 corpus to be
/// able to fail an early stop.
fn n_tail_threshold() -> usize {
    30
}
