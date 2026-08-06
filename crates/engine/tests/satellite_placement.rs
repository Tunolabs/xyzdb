//! Sub-gravity (satellite) axis — PLACEMENT + BOUNDED READ phase (Tanda 2).
//!
//! Tanda 1 shipped the inert declaration. This phase activates it: the write
//! path places each record in the satellite `hash16(field_value)` of its gravity
//! bucket, and the read path narrows a `WHERE gravity AND satellite_field` scan
//! to that one satellite sub-range, keeping the field predicate as an
//! anti-collision residual. These are the gates that make the win a guarantee,
//! not a promise — especially the two nobody writes by default: the hash16
//! COLLISION (proving the residual earns its keep) and the sat-0 DUMPSTER.
//!
//! The global test knobs (`SAT_FORCE_PARENT_SCAN`, `SAT_SKIP_ANTICOLLISION_
//! RESIDUAL`) are process-wide, so every test here serialises on `GATE_LOCK` and
//! resets both knobs on entry and on drop (panic-safe).

// SPDX-License-Identifier: BUSL-1.1
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use xyzdb_core::key::hash_to_16bits;
use xyzdb_core::record::Record;
use xyzdb_engine::engine::{
    Engine, QueryResult, SAT_FORCE_PARENT_SCAN, SAT_SKIP_ANTICOLLISION_RESIDUAL,
};

static GATE_LOCK: Mutex<()> = Mutex::new(());

/// Reset both knobs to their production state (off). Runs on drop so a panicking
/// test never leaks a knob into the next one.
struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        SAT_FORCE_PARENT_SCAN.store(false, Relaxed);
        SAT_SKIP_ANTICOLLISION_RESIDUAL.store(false, Relaxed);
    }
}

/// Acquire the serialising lock and force both knobs off. Hold the returned
/// guard for the whole test body.
fn gate() -> (std::sync::MutexGuard<'static, ()>, KnobReset) {
    let g = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SAT_FORCE_PARENT_SCAN.store(false, Relaxed);
    SAT_SKIP_ANTICOLLISION_RESIDUAL.store(false, Relaxed);
    (g, KnobReset)
}

fn run(engine: &Engine, s: &str) -> Result<QueryResult, String> {
    engine.run(s).map_err(|e| format!("{s:?}: {e:?}"))
}

/// Records returned by a query, in emission order.
fn records(engine: &Engine, q: &str) -> Vec<Record> {
    match run(engine, q).unwrap() {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected result for {q:?}: {other:?}"),
    }
}

/// The `n` field of each record, in order — a compact identity for sequence
/// comparison.
fn ns(recs: &[Record]) -> Vec<i64> {
    recs.iter()
        .map(|r| r.fields.get("n").and_then(|v| v.as_int()).unwrap())
        .collect()
}

/// Brute-force two distinct strings whose `hash16` collides (found in
/// microseconds — the whole point of the 16-bit axis is that this exists).
fn find_hash16_collision() -> (String, String) {
    let mut seen: std::collections::HashMap<u16, String> = std::collections::HashMap::new();
    for i in 0..1_000_000u32 {
        let s = format!("kind-{i}");
        let h = hash_to_16bits(&s);
        if let Some(prev) = seen.insert(h, s.clone()) {
            return (prev, s);
        }
    }
    panic!("no hash16 collision found (impossible for 65536 buckets)");
}

/// Brute-force a string whose `hash16` is exactly 0 (the default/dumpster sat).
fn find_hash16_zero() -> String {
    for i in 0..1_000_000u32 {
        let s = format!("zero-{i}");
        if hash_to_16bits(&s) == 0 {
            return s;
        }
    }
    panic!("no hash16==0 value found");
}

/// A lobe with gravity `scope` and satellite `kind`, on which the bounded path
/// activates for `WHERE scope = ... AND kind = ...`.
fn seeded_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "events""#).unwrap();
    run(&engine, r#"GRAVITY BY scope IN "events""#).unwrap();
    run(&engine, r#"SATELLITE BY kind IN "events""#).unwrap();
    (dir, engine)
}

/// Placement + detection agree: a record written under a satellite axis is found
/// by a bounded query on its value. If placement (write) and detection (read)
/// canonicalised the value differently, this would return nothing.
#[test]
fn bounded_query_finds_its_records() {
    let _gate = gate();
    let (_dir, engine) = seeded_engine();
    for i in 0..30 {
        let kind = if i % 3 == 0 { "click" } else { "view" };
        run(
            &engine,
            &format!(r#"PUT {{scope: "s1", kind: "{kind}", n: {i}}} IN "events""#),
        )
        .unwrap();
    }
    let got = records(
        &engine,
        r#"SCAN "events" WHERE scope = "s1" AND kind = "click""#,
    );
    let mut got_ns = ns(&got);
    got_ns.sort();
    assert_eq!(got_ns, vec![0, 3, 6, 9, 12, 15, 18, 21, 24, 27]);
}

/// G1 — route equivalence (pure optimisation). The SAME query via the bounded
/// satellite range and via the forced parent-bucket scan must return the SAME
/// rows in the SAME order.
#[test]
fn bounded_route_equals_parent_route_row_for_row() {
    let _gate = gate();
    let (_dir, engine) = seeded_engine();
    for i in 0..60 {
        let kind = match i % 3 {
            0 => "click",
            1 => "view",
            _ => "hover",
        };
        run(
            &engine,
            &format!(r#"PUT {{scope: "s1", kind: "{kind}", n: {i}}} IN "events""#),
        )
        .unwrap();
    }
    let q = r#"SCAN "events" WHERE scope = "s1" AND kind = "view""#;

    // Bounded (default).
    let bounded = ns(&records(&engine, q));
    // Forced parent scan + residual.
    SAT_FORCE_PARENT_SCAN.store(true, Relaxed);
    let parent = ns(&records(&engine, q));
    SAT_FORCE_PARENT_SCAN.store(false, Relaxed);

    assert!(!bounded.is_empty(), "the query must return rows");
    assert_eq!(
        bounded, parent,
        "bounded satellite scan diverged from the parent scan (sequence, not just set)"
    );
}

/// G2 — the collision gate (the central one). Two `kind` values that collide in
/// hash16 share a satellite; a bounded query for one must return ONLY its rows.
/// Negative control: with the anti-collision residual disabled, the intruder
/// from the colliding value leaks — the assertion must then fail.
#[test]
fn hash16_collision_is_resolved_by_the_residual() {
    let _gate = gate();
    let (a, b) = find_hash16_collision();
    assert_eq!(
        hash_to_16bits(&a),
        hash_to_16bits(&b),
        "precondition: the two values collide"
    );
    assert_ne!(a, b);

    let (_dir, engine) = seeded_engine();
    // 5 rows of value `a`, 5 of value `b`, all in the same gravity bucket and
    // (by the collision) the same satellite.
    for i in 0..5 {
        run(
            &engine,
            &format!(r#"PUT {{scope: "s1", kind: "{a}", n: {i}}} IN "events""#),
        )
        .unwrap();
    }
    for i in 100..105 {
        run(
            &engine,
            &format!(r#"PUT {{scope: "s1", kind: "{b}", n: {i}}} IN "events""#),
        )
        .unwrap();
    }
    let q = format!(r#"SCAN "events" WHERE scope = "s1" AND kind = "{a}""#);

    // With the residual (production): exactly a's rows, none of b's.
    let mut got = ns(&records(&engine, &q));
    got.sort();
    assert_eq!(got, vec![0, 1, 2, 3, 4], "residual must drop the collider");

    // Negative control: disable the residual → b's rows (same satellite) leak in.
    SAT_SKIP_ANTICOLLISION_RESIDUAL.store(true, Relaxed);
    let leaked = ns(&records(&engine, &q));
    SAT_SKIP_ANTICOLLISION_RESIDUAL.store(false, Relaxed);
    assert!(
        leaked.len() > 5,
        "negative control: without the residual the collider must leak (got {} rows), \
         proving the residual is what enforces exactness and that both values really \
         share the satellite",
        leaked.len()
    );
}

/// G3 — the sat-0 dumpster. A record MISSING the satellite field lands in sat 0,
/// alongside any value whose hash16 is 0. A bounded query for that hash16==0
/// value must return only the value's row, not the field-less one — the residual
/// is mandatory on sat 0 because unrelated rows share it.
#[test]
fn sat_zero_dumpster_needs_the_residual() {
    let _gate = gate();
    let zero_val = find_hash16_zero();
    assert_eq!(hash_to_16bits(&zero_val), 0);

    let (_dir, engine) = seeded_engine();
    // A record with NO kind field → sat 0.
    run(&engine, r#"PUT {scope: "s1", n: 1} IN "events""#).unwrap();
    // A record whose kind hashes to 0 → also sat 0.
    run(
        &engine,
        &format!(r#"PUT {{scope: "s1", kind: "{zero_val}", n: 2}} IN "events""#),
    )
    .unwrap();

    let got = records(
        &engine,
        &format!(r#"SCAN "events" WHERE scope = "s1" AND kind = "{zero_val}""#),
    );
    assert_eq!(
        ns(&got),
        vec![2],
        "only the record whose kind matches must return; the field-less sat-0 \
         neighbour must be dropped by the residual"
    );
}

/// G4 — the count win. `SCAN WHERE gravity AND satellite_field | AGGREGATE
/// count()` returns the exact number, and equals the forced-parent count (same
/// answer, bounded route).
#[test]
fn count_over_satellite_is_exact_and_matches_parent() {
    let _gate = gate();
    let (_dir, engine) = seeded_engine();
    for i in 0..90 {
        let kind = if i % 3 == 0 { "click" } else { "view" };
        run(
            &engine,
            &format!(r#"PUT {{scope: "s1", kind: "{kind}", n: {i}}} IN "events""#),
        )
        .unwrap();
    }
    let q = r#"SCAN "events" WHERE scope = "s1" AND kind = "click" | AGGREGATE count()"#;

    let count_of = |engine: &Engine| -> i64 {
        match run(engine, q).unwrap() {
            QueryResult::Aggregation(a) => a.get("count").and_then(|v| v.as_int()).unwrap(),
            other => panic!("unexpected aggregate result: {other:?}"),
        }
    };

    let bounded = count_of(&engine);
    assert_eq!(bounded, 30, "exact count of the satellite's matches");

    SAT_FORCE_PARENT_SCAN.store(true, Relaxed);
    let parent = count_of(&engine);
    SAT_FORCE_PARENT_SCAN.store(false, Relaxed);
    assert_eq!(
        bounded, parent,
        "bounded count must equal the parent-scan count"
    );
}

/// Re-satellite on SET: a SET that changes the satellite field MOVES the record
/// to its new satellite (mirroring re-gravitation), so a bounded query on the
/// new value finds it and one on the old value does not.
#[test]
fn set_moves_record_to_new_satellite() {
    let _gate = gate();
    let (_dir, engine) = seeded_engine();
    run(
        &engine,
        r#"PUT {scope: "s1", kind: "click", n: 1} IN "events""#,
    )
    .unwrap();
    // Move the satellite field.
    run(
        &engine,
        r#"SET "events" kind = "view" WHERE scope = "s1" AND kind = "click""#,
    )
    .unwrap();

    let at_new = ns(&records(
        &engine,
        r#"SCAN "events" WHERE scope = "s1" AND kind = "view""#,
    ));
    assert_eq!(
        at_new,
        vec![1],
        "record must be found under its new satellite"
    );

    let at_old = records(
        &engine,
        r#"SCAN "events" WHERE scope = "s1" AND kind = "click""#,
    );
    assert!(
        at_old.is_empty(),
        "record must NOT remain in the old satellite after SET moved it"
    );
}

// ─── NEAREST bounded by satellite — the original-ticket query ────────────────

/// Format a float vector as a xyTalk list literal `[f, f, ...]`.
fn vec_lit(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|f| format!("{f:?}")).collect();
    format!("[{}]", parts.join(", "))
}

/// The `kind` of each record, in order.
fn kinds(recs: &[Record]) -> Vec<String> {
    recs.iter()
        .map(|r| {
            r.fields
                .get("kind")
                .and_then(|v| v.as_text())
                .unwrap()
                .to_string()
        })
        .collect()
}

/// The LID string of each record, in order.
fn lids(recs: &[Record]) -> Vec<String> {
    recs.iter().map(|r| r.lid.to_string()).collect()
}

/// A vector lobe with gravity `scope`, satellite `kind`, searchable `emb`, seeded
/// so that: 10 `kind = a` records lie at varying (increasing) distance from the
/// query, and ONE `kind = b` intruder (b collides with a in hash16) has a vector
/// IDENTICAL to the query — so without the residual it would rank #1. Returns the
/// query vector.
fn seeded_vector_engine(a: &str, b: &str) -> (tempfile::TempDir, Engine, Vec<f32>) {
    const DIM: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "docs""#).unwrap();
    run(&engine, r#"GRAVITY BY scope IN "docs""#).unwrap();
    run(&engine, r#"SATELLITE BY kind IN "docs""#).unwrap();
    run(&engine, r#"VECTOR emb IN "docs""#).unwrap();

    let mut query = vec![0.0f32; DIM];
    query[0] = 1.0;

    for i in 0..10 {
        let mut v = vec![0.0f32; DIM];
        v[0] = 1.0;
        v[1] = 0.1 * (i as f32 + 1.0); // farther from query as i grows
        run(
            &engine,
            &format!(
                r#"PUT {{scope: "s1", kind: "{a}", n: {i}, emb: {}}} IN "docs""#,
                vec_lit(&v)
            ),
        )
        .unwrap();
    }
    // Intruder: same gravity bucket, colliding satellite, vector == query.
    run(
        &engine,
        &format!(
            r#"PUT {{scope: "s1", kind: "{b}", n: 999, emb: {}}} IN "docs""#,
            vec_lit(&query)
        ),
    )
    .unwrap();
    (dir, engine, query)
}

/// NEAREST is bounded to the satellite: `SCAN … WHERE gravity AND kind | NEAREST`
/// scores only the satellite, so the answer is the exact top-k of the filtered
/// set — identical (lids and order) to the forced full-bucket path. This is the
/// query that opened the ticket; it is now bounded, not scoring the whole bucket.
#[test]
fn nearest_bounded_equals_parent_route() {
    let _gate = gate();
    let (a, b) = find_hash16_collision();
    let (_dir, engine, query) = seeded_vector_engine(&a, &b);
    let q = format!(
        r#"SCAN "docs" WHERE scope = "s1" AND kind = "{a}" | NEAREST(emb, {}, 5, cosine)"#,
        vec_lit(&query)
    );

    let bounded = records(&engine, &q);
    assert_eq!(bounded.len(), 5, "top-5 of the satellite");
    assert!(
        kinds(&bounded).iter().all(|k| k == &a),
        "bounded NEAREST must return only kind={a}, never the colliding intruder"
    );

    SAT_FORCE_PARENT_SCAN.store(true, Relaxed);
    let parent = records(&engine, &q);
    SAT_FORCE_PARENT_SCAN.store(false, Relaxed);
    assert_eq!(
        lids(&bounded),
        lids(&parent),
        "bounded NEAREST top-k must equal the full-bucket path row for row"
    );
}

/// NEAREST negative control: the intruder's vector is identical to the query, so
/// without the anti-collision residual it ranks #1 and leaks into the top-k. With
/// the residual (production) it is dropped. Proves the residual is what keeps a
/// hash16 collider out of the NEAREST answer.
#[test]
fn nearest_residual_keeps_collider_out_of_topk() {
    let _gate = gate();
    let (a, b) = find_hash16_collision();
    let (_dir, engine, query) = seeded_vector_engine(&a, &b);
    let q = format!(
        r#"SCAN "docs" WHERE scope = "s1" AND kind = "{a}" | NEAREST(emb, {}, 5, cosine)"#,
        vec_lit(&query)
    );

    // Production: residual on → no intruder in the top-k.
    let on = records(&engine, &q);
    assert!(
        !kinds(&on).contains(&b),
        "with the residual, the collider must not appear"
    );

    // Negative control: residual off → the identical-vector intruder ranks #1.
    SAT_SKIP_ANTICOLLISION_RESIDUAL.store(true, Relaxed);
    let off = records(&engine, &q);
    SAT_SKIP_ANTICOLLISION_RESIDUAL.store(false, Relaxed);
    assert!(
        kinds(&off).contains(&b),
        "negative control: without the residual the closest-vector collider must leak \
         into the NEAREST top-k"
    );
}
