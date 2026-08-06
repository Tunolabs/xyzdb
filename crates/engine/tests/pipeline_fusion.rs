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

// SPDX-License-Identifier: BUSL-1.1
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

// ─── Every continuation, not just the one that found the bug ─────────────────
//
// B changed the plan for ANY pipeline starting with `SCAN | NEAREST`, so every
// step that can follow changed which candidate set it sees. `SHAPE` above is one
// of eight. The rest are not variations on it: `AGGREGATE` folds over the set
// (so a wrong set is a wrong NUMBER, not a wrong list), `FOLLOW` leaves for
// another lobe carrying it, and `SET`/`DELETE` WRITE based on it.
//
// The shared corpus below puts the best matches in the TAIL, past the page the
// unfused plan would have stopped at, so any step that saw the page instead of
// the bucket reports a different answer — and the premise is asserted, not
// assumed.

/// One gravity bucket, `n` rows, `x` ascending with similarity: the higher the
/// `x`, the closer to the query. Whatever a step reports about `x` therefore says
/// which candidate set it was handed.
fn seed_tail_heavy(e: &Engine, n: usize) -> String {
    exec(e, r#"LOBE "v""#);
    exec(e, r#"GRAVITY BY bucket IN "v""#);
    exec(e, r#"VECTOR emb IN "v""#);
    for i in 0..n {
        let mut dims = vec!["0.0".to_string(); DIM];
        dims[0] = format!("{:.6}", i as f64 / n as f64);
        dims[1] = format!("{:.6}", 1.0 - i as f64 / n as f64);
        exec(
            e,
            &format!(
                r#"PUT {{_type:"R", id:"g{i}", x:{i}, doc:"d{}", bucket:"0", emb:[{}]}} IN "v""#,
                i % 7,
                dims.join(", ")
            ),
        );
    }
    let page = records(exec(e, r#"SCAN "v" WHERE bucket = "0""#));
    assert!(
        page.len() < n,
        "premise broken: a bare SCAN returned {} of {n} rows, so it is not \
         truncating and none of these tests can observe the defect",
        page.len()
    );
    let mut q = vec!["0.0".to_string(); DIM];
    q[0] = "1.0".to_string();
    q.join(", ")
}

const N: usize = 2_400;

/// `AGGREGATE` is the case a projection test cannot stand in for: it folds the
/// set into a number, so seeing the wrong set is a wrong TOTAL rather than a
/// visibly wrong list — nothing downstream can notice.
///
/// `max(x)`, not `count()`: the unfused plan also returned k records, so a count
/// is identical under both plans and would be a green that cannot fail. The
/// instrument has to be able to see the effect before its reading means anything.
#[test]
fn aggregate_after_nearest_folds_the_true_top_k() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);

    let agg = exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" | NEAREST 10 BY emb TO [{q}] USING cosine | AGGREGATE max(x)"#
        ),
    );
    let max = match &agg {
        QueryResult::Aggregation(m) => m.values().next().cloned(),
        other => panic!("expected an aggregation, got {other:?}"),
    };
    // `max` returns a Float even over an Int column — the min/max return type is a
    // deferred open contract, so this compares the VALUE and stays agnostic about
    // which numeric variant carries it. Pinning the variant here would make this
    // test fail for a reason that has nothing to do with the plan it gates.
    let got = match max {
        Some(Value::Float(f)) => f,
        Some(Value::Int(i)) => i as f64,
        other => panic!("max(x) returned neither Int nor Float: {other:?}"),
    };
    assert_eq!(
        got,
        (N - 1) as f64,
        "the fold must run over the top-k of the whole bucket; the closest row is \
         the last one written, which the first page never contains"
    );
}

/// `TAKE` truncates the ranked list — of the right list.
#[test]
fn take_after_nearest_truncates_the_true_top_k() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);

    let ids: Vec<String> = records(exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" | NEAREST 10 BY emb TO [{q}] USING cosine | TAKE 3"#
        ),
    ))
    .iter()
    .map(|r| match r.fields.get("id") {
        Some(Value::Text(s)) => s.clone(),
        other => panic!("missing id: {other:?}"),
    })
    .collect();
    assert_eq!(
        ids,
        vec![
            format!("g{}", N - 1),
            format!("g{}", N - 2),
            format!("g{}", N - 3)
        ],
        "TAKE must cut the fused ranking, not a ranking of the first page"
    );
}

/// `FOLLOW` leaves for another lobe carrying the set with it, so a wrong set
/// becomes a wrong set of DOCUMENTS — a step further from anything the caller
/// could sanity-check.
#[test]
fn follow_after_nearest_crosses_from_the_true_top_k() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);
    // The referenced lobe: one document per `doc` value, anchored so FOLLOW can
    // resolve it.
    exec(&e, r#"LOBE "docs""#);
    exec(&e, r#"GRAVITY BY doc_id IN "docs""#);
    for d in 0..7 {
        exec(
            &e,
            &format!(r#"PUT {{_type:"D", doc_id:"d{d}", title:"t{d}"}} IN "docs""#),
        );
    }

    let followed = records(exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" | NEAREST 3 BY emb TO [{q}] USING cosine | FOLLOW doc TO "docs" ON doc_id"#
        ),
    ));
    let mut titles: Vec<String> = followed
        .iter()
        .filter_map(|r| match r.fields.get("title") {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    titles.sort();
    titles.dedup();
    // Rows 2399, 2398, 2397 carry doc d(2399%7)=d5, d4, d3.
    assert_eq!(
        titles,
        vec!["t3".to_string(), "t4".to_string(), "t5".to_string()],
        "FOLLOW must cross from the fused top-k; the page's top-3 references \
         different documents"
    );
}

/// `SET` WRITES based on the set. Getting this wrong does not return a wrong
/// answer — it stores one.
#[test]
fn set_after_nearest_writes_the_true_top_k() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);

    exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" | NEAREST 3 BY emb TO [{q}] USING cosine | SET tagged = "yes""#
        ),
    );
    let tagged = records(exec(
        &e,
        r#"SCAN "v" WHERE bucket = "0" AND tagged = "yes" LIMIT 100"#,
    ));
    let mut ids: Vec<String> = tagged
        .iter()
        .filter_map(|r| match r.fields.get("id") {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            format!("g{}", N - 3),
            format!("g{}", N - 2),
            format!("g{}", N - 1)
        ],
        "SET must mark the fused top-k — a wrong set here is persisted, not just returned"
    );
}

/// `DELETE` is `SET`'s sharper twin: the wrong set is destroyed, not marked.
#[test]
fn delete_after_nearest_removes_the_true_top_k() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);

    exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" | NEAREST 3 BY emb TO [{q}] USING cosine | DELETE"#
        ),
    );
    for gone in [N - 1, N - 2, N - 3] {
        let hit = records(exec(
            &e,
            &format!(r#"SCAN "v" WHERE bucket = "0" AND id = "g{gone}" LIMIT 5"#),
        ));
        assert!(
            hit.is_empty(),
            "g{gone} was in the fused top-3 and must be gone"
        );
    }
    let survivor = records(exec(
        &e,
        &format!(
            r#"SCAN "v" WHERE bucket = "0" AND id = "g{}" LIMIT 5"#,
            N - 4
        ),
    ));
    assert_eq!(
        survivor.len(),
        1,
        "g{} ranked 4th and must survive — the boundary is what proves DELETE cut \
         where the ranking said, not somewhere else",
        N - 4
    );
}

/// `GROUP BY` after `NEAREST` is refused, and was before. Asserted so the
/// refusal is a decision on record rather than an accident nobody re-checks.
#[test]
fn group_by_after_nearest_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);

    let r = e.run(&format!(
        r#"SCAN "v" WHERE bucket = "0" | NEAREST 10 BY emb TO [{q}] USING cosine | GROUP BY doc | AGGREGATE count()"#
    ));
    assert!(
        r.is_err(),
        "GROUP BY is only supported as SCAN | GROUP BY … | AGGREGATE …; after \
         NEAREST it must error rather than quietly group something else"
    );
}

/// `PULL` — stated, not faked.
///
/// PULL re-expands each hit to its co-located neighbourhood, which IS the gravity
/// bucket. Both plans rank inside one bucket, so both PULLs expand to the same
/// bucket: **this step cannot distinguish the two plans by construction**. The
/// test asserts the only thing that is real here — that PULL still composes and
/// returns the bucket — and says why it is not a discriminating gate, instead of
/// dressing an always-true comparison up as coverage.
#[test]
fn pull_after_nearest_composes_but_cannot_discriminate_the_plans() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    let q = seed_tail_heavy(&e, N);

    let pulled = records(exec(
        &e,
        &format!(r#"SCAN "v" WHERE bucket = "0" | NEAREST 3 BY emb TO [{q}] USING cosine | PULL"#),
    ));
    assert!(
        pulled.len() >= 3,
        "PULL expands the hits to their co-located context, so it returns at \
         least the hits themselves"
    );
}
