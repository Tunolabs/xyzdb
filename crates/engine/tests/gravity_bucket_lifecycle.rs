//! 0.7.5 gravity-bucket read-path fixes.
//!
//! Pre-0.7.5, PUT wrote a per-value gravity dictionary entry
//! ((field, value) → ONE LID, overwritten on every PUT) and unlimited
//! FIND resolved gravity predicates through it — returning at most one of
//! the bucket's N records. PULL scanned the 48-bit bucket with no
//! collision post-filter, so records of a different gravity value that
//! hashed into the same bucket leaked into the result.
//!
//! These tests pin the fixed semantics: FIND without LIMIT returns the
//! full bucket (parity with SCAN), and PULL keeps linked children while
//! rejecting bucket cohabitants that carry a different gravity value.

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

fn records(qr: QueryResult) -> Vec<xyzdb_core::record::Record> {
    match qr {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn find_without_limit_returns_the_full_gravity_bucket() {
    const PER_CLIENT: usize = 50;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "creditos""#);

    // Two buckets so the scan has neighbours to NOT return.
    for c in ["C1", "C2"] {
        for i in 0..PER_CLIENT {
            exec(
                &engine,
                &format!(r#"PUT {{_type: "Credit", *cliente_id: "{c}", n: {i}}} IN "creditos""#),
            );
        }
    }

    // Unlimited FIND must return every record of the bucket, not 1.
    let found = records(exec(&engine, r#"FIND "creditos" WHERE cliente_id = "C1""#));
    assert_eq!(
        found.len(),
        PER_CLIENT,
        "unlimited FIND must return the full gravity bucket"
    );

    // Parity with SCAN on the same predicate.
    let scanned = records(exec(&engine, r#"SCAN "creditos" WHERE cliente_id = "C1""#));
    assert_eq!(
        found.len(),
        scanned.len(),
        "FIND and SCAN must agree on a gravity Eq predicate"
    );

    // Extra non-gravity predicate still applies in-range.
    let narrowed = records(exec(
        &engine,
        r#"FIND "creditos" WHERE cliente_id = "C1" AND n = 7"#,
    ));
    assert_eq!(
        narrowed.len(),
        1,
        "secondary predicate must filter in-range"
    );

    // Absent value → empty, not an error and not a stale single record.
    let absent = records(exec(
        &engine,
        r#"FIND "creditos" WHERE cliente_id = "NOPE""#,
    ));
    assert!(
        absent.is_empty(),
        "absent gravity value must return nothing"
    );
}

/// PULL's collision guard must NOT change cluster semantics: same-value
/// members, LINK TO children without the gravity field, and LINK TO
/// children carrying their OWN gravity value (link overrides gravity)
/// all stay. The drop case — a record whose own value re-hashes to this
/// same bucket (true 48-bit collision) — can't be staged with real
/// strings; it is pinned by the `is_collision_victim` unit tests in
/// `ops/pull.rs`, which inject the bucket hash directly.
#[test]
fn pull_keeps_cluster_members_linked_children_and_link_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    exec(&engine, r#"LOBE "clientes""#);

    // Entity A: parent + two same-value children.
    exec(
        &engine,
        r#"PUT {_type: "Client", *cliente_id: "A", name: "Alice"} IN "clientes""#,
    );
    for i in 0..2 {
        exec(
            &engine,
            &format!(r#"PUT {{_type: "Credit", *cliente_id: "A", n: {i}}} IN "clientes""#),
        );
    }

    // Linked child WITHOUT the gravity field: inherits A's bucket via
    // LINK TO; nothing to verify against → kept.
    exec(
        &engine,
        r#"PUT {_type: "Note", txt: "hello"} IN "clientes" LINK TO "clientes" WHERE cliente_id = "A" AS "nota""#,
    );

    // Linked child CARRYING its own gravity value: LINK overrides gravity,
    // and its value re-hashes to a different bucket → deliberate placement,
    // kept (the pre-existing test_gravity_link_overrides contract).
    exec(
        &engine,
        r#"PUT {_type: "Stray", *cliente_id: "B"} IN "clientes" LINK TO "clientes" WHERE cliente_id = "A" AS "stray""#,
    );

    let pulled = records(exec(
        &engine,
        r#"FIND "clientes" WHERE cliente_id = "A" AND _type = "Client" | PULL depth=1"#,
    ));

    let types: Vec<&str> = pulled
        .iter()
        .filter_map(|r| match r.fields.get("_type") {
            Some(xyzdb_core::value::Value::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        types.iter().filter(|t| **t == "Credit").count() == 2,
        "PULL must return both same-value children, got {types:?}"
    );
    assert!(
        types.contains(&"Note"),
        "PULL must keep the LINK TO child without the gravity field, got {types:?}"
    );
    assert!(
        types.contains(&"Stray"),
        "PULL must keep LINK TO children carrying their own gravity value, got {types:?}"
    );
}
