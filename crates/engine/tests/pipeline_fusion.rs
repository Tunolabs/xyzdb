//! The fused `SCAN | NEAREST` prefix must survive a longer pipeline.
//!
//! `SCAN | NEAREST` is a fused plan: it ranks the WHOLE gravity bucket through
//! the hoisted vector column. A third step used to drop the query into the
//! generic loop, where `SCAN` materialises one default page
//! (`SCAN_LIMIT_DEFAULT` = 1000) and `NEAREST` ranks inside it — so appending a
//! step changed which records came back.
//!
//! These tests gate the plan CHOICE, which is why they are their own file: the
//! ghost hydration fix and this one are separate risks, and a test that names
//! the wrong one is a test nobody re-reads when the other changes.

use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

const DIM: usize = 64;

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine.run(s).unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(rs) => rs,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("expected records, got {other:?}"),
    }
}

/// Appending a projection must not change the answer.
///
/// `SCAN | NEAREST` fuses into the vector-prefix path and ranks the whole
/// bucket. Adding a third step used to drop the query into the generic loop,
/// where `SCAN` materialises one default page (1000 records) and `NEAREST`
/// ranks inside it — so `| SHAPE {id}`, which the spec defines as shaping the
/// field set and NOT which records come back, silently changed which records
/// came back. On the benchmark corpus the two answers shared no ids at all.
///
/// The assertion is the equality of the two id lists, not that the shaped query
/// returned k rows: a wrong top-k also returns k rows.
#[test]
fn a_trailing_projection_does_not_change_the_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    exec(&e, r#"LOBE "v""#);
    exec(&e, r#"GRAVITY BY bucket IN "v""#);
    exec(&e, r#"VECTOR emb IN "v""#);
    // Well past the 1000-record default page, and the best matches live in the
    // tail: a plan that only ever sees the first page cannot find them.
    let n = 2_400;
    for i in 0..n {
        let mut dims = vec!["0.0".to_string(); DIM];
        dims[0] = format!("{:.6}", i as f64 / n as f64);
        dims[1] = format!("{:.6}", 1.0 - i as f64 / n as f64);
        exec(
            &e,
            &format!(
                r#"PUT {{_type:"R", id:"g{i}", bucket:"0", emb:[{}]}} IN "v""#,
                dims.join(", ")
            ),
        );
    }
    // PREMISE, asserted rather than assumed: the bucket must exceed the page the
    // unfused plan would stop at. A bucket smaller than the page ranks identically
    // under both plans, so the same test body over (say) 493 rows is a green that
    // cannot fail — it would pass with this fix reverted. Reading the truncation
    // back from the engine also survives a future change to the default.
    let page = records(exec(&e, r#"SCAN "v" WHERE bucket = "0""#));
    assert!(
        page.len() < n,
        "premise broken: a bare SCAN returned {} of {n} rows, so it is not \
         truncating and this test cannot observe the defect it exists for",
        page.len()
    );

    let mut q = vec!["0.0".to_string(); DIM];
    q[0] = "1.0".to_string();
    let q = q.join(", ");

    let ids = |qr: QueryResult| -> Vec<String> {
        records(qr)
            .iter()
            .map(|r| match r.fields.get("id") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("missing id: {other:?}"),
            })
            .collect()
    };

    let fused = ids(exec(
        &e,
        &format!(r#"SCAN "v" WHERE bucket = "0" | NEAREST 5 BY emb TO [{q}] USING cosine"#),
    ));
    let shaped = ids(exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" | NEAREST 5 BY emb TO [{q}] USING cosine | SHAPE {{id}}"#
        ),
    ));
    assert_eq!(
        shaped, fused,
        "a projection must not change WHICH records come back, nor their order"
    );
    assert_eq!(
        fused.first().map(String::as_str),
        Some(format!("g{}", n - 1).as_str()),
        "the closest vector is the last one written — a plan that stops at the \
         first page can never reach it"
    );
}
