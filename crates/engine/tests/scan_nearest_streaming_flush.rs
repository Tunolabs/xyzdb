//! Gate: the fused `[Scan, Nearest]` path is bit-identical to the materialized
//! full path when the gravity bucket spans FLUSHED SSTables + the active
//! memtable — i.e. when `range_stream` actually streams across SSTable blocks,
//! not just the in-memory memtable the other fused gates exercise.
//!
//! This is the "streaming == materialized-all, same exact top-k" contract for
//! the change that swapped `range` (collect the whole bucket) → `range_stream`
//! (lazy, O(block) working set) in `ops::nearest::try_prefix_scan_nearest`.
//!
//! SINGLE test in its own binary: it shrinks the memtable via env BEFORE opening
//! the engine so a modest seed forces the `vectors` keyspace to flush several
//! SSTables (COMPACT does NOT flush `vectors`, only spatial/identity/dictionary).
//! Own process → no concurrent env access (same rationale as q5_scale_repro).

// SPDX-License-Identifier: BUSL-1.1
use xytalk_parser::ast::{PipelineStep, Statement};
use xyzdb_core::lid::LID;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

const DIM: usize = 64;

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(recs) => recs,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected Records, got {other:?}"),
    }
}

fn lids(qr: QueryResult) -> Vec<LID> {
    records(qr).into_iter().map(|r| r.lid).collect()
}

fn ids(qr: QueryResult) -> Vec<String> {
    records(qr)
        .into_iter()
        .map(|r| match r.fields.get("id") {
            Some(Value::Text(t)) => t.clone(),
            other => panic!("record without id: {other:?}"),
        })
        .collect()
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
    format!("[{}]", parts.join(", "))
}

/// The SAME query through the FORCED full path (materializes the whole bucket).
fn full_path(engine: &Engine, query: &str) -> Vec<LID> {
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
    xyzdb_engine::ops::nearest::execute_nearest(recs, &nearest)
        .expect("forced full-path nearest")
        .into_iter()
        .map(|r| r.lid)
        .collect()
}

#[test]
fn fused_streaming_matches_full_path_across_sstable_flush() {
    // Shrink EVERY keyspace memtable before the engine (and its bg threads) start
    // so the `vectors` keyspace seals + flushes several SSTables during the seed.
    // SAFETY: single-threaded, only test in this binary, set before Engine::open.
    unsafe {
        std::env::set_var("TURBA_TEST_MEMTABLE_BYTES", "16384");
    }

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "mem""#);
    exec(&engine, r#"VECTOR emb IN "mem""#);

    // ~180 vectors in ONE bucket: with a 16 KB memtable and 64-d columns this
    // crosses the flush threshold several times → the bucket lives across
    // multiple SSTables. Includes exact ties (shared vector) so the (score DESC,
    // lid ASC) tiebreak is exercised while streaming.
    let mut tied = vec![0.0f32; DIM];
    tied[0] = 1.0;
    tied[1] = 1.0;
    for i in 0..8 {
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"tie{i}", emb:{}}} IN "mem""#,
                vec_literal(&tied)
            ),
        );
    }
    for i in 0..170 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[1] = 1.0;
        v[(i % (DIM - 2)) + 2] = 0.02 + (i as f32) * 0.003; // spread + near-ties
        exec(
            &engine,
            &format!(
                r#"PUT {{*conv:"c1", id:"n{i}", emb:{}}} IN "mem""#,
                vec_literal(&v)
            ),
        );
    }
    // Different bucket — must never surface through the streamed range.
    exec(
        &engine,
        &format!(
            r#"PUT {{*conv:"c2", id:"intruder", emb:{}}} IN "mem""#,
            vec_literal(&tied)
        ),
    );

    let mut q = vec![0.0f32; DIM];
    q[0] = 1.0;
    q[1] = 1.0;
    for metric in ["cosine", "dot", "l2"] {
        for k in [1u64, 3, 10, 50, 200] {
            let query = format!(
                r#"SCAN "mem" WHERE conv="c1" | NEAREST(emb, {}, {k}, {metric})"#,
                vec_literal(&q)
            );
            let fused = lids(exec(&engine, &query));
            let full = full_path(&engine, &query);
            assert_eq!(
                fused, full,
                "fused streaming path != full (materialized) path across flush, \
                 metric={metric} k={k} — streaming dropped/reordered a candidate"
            );
            let fused_ids = ids(exec(&engine, &query));
            assert!(
                !fused_ids.iter().any(|id| id == "intruder"),
                "cross-bucket record leaked through the streamed range: {fused_ids:?}"
            );
        }
    }

    // No silent drop: a k covering the whole bucket returns every planted record
    // (178 = 8 ties + 170 neighbours), proving streaming enumerated the full
    // bucket, not a truncated prefix.
    let big = format!(
        r#"SCAN "mem" WHERE conv="c1" | NEAREST(emb, {}, 500, dot)"#,
        vec_literal(&q)
    );
    assert_eq!(
        lids(exec(&engine, &big)).len(),
        178,
        "streaming enumerated fewer than the 178 planted records — a bucket entry was dropped"
    );
}
