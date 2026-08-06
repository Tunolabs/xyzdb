//! Sub-gravity (satellite) axis — DECLARATION phase (Tanda 1).
//!
//! This phase adds the declaration surface only: `SATELLITE BY <field> IN
//! "<lobe>"` parses, persists (D1: seal+flush), reloads at boot, refuses a
//! non-empty lobe (§6), and holds one axis per lobe (§7.1). Placement is NOT
//! activated: every record still lands at `sat = 0`, so a declared satellite is
//! behaviourally inert. These tests pin exactly that: the declaration is durable
//! and guarded, and it does not perturb the normal write/read path.
//!
//! The persistence+boot proof is observable without engine internals: after a
//! clean reopen, re-declaring a DIFFERENT axis on the same lobe must error with
//! "already has a satellite axis" — which can only happen if the spec was
//! loaded from disk at boot (a fresh, unloaded lobe would accept it).

// SPDX-License-Identifier: BUSL-1.1
use xyzdb_core::record::Record;
use xyzdb_engine::engine::{Engine, QueryResult};

fn run(engine: &Engine, s: &str) -> Result<QueryResult, String> {
    engine.run(s).map_err(|e| format!("{s:?}: {e:?}"))
}

fn scan(engine: &Engine, lobe: &str) -> Vec<Record> {
    match run(engine, &format!(r#"SCAN "{lobe}""#)).unwrap() {
        QueryResult::Records(r) => r,
        QueryResult::PaginatedRecords { records, .. } => records,
        other => panic!("unexpected scan result: {other:?}"),
    }
}

/// Declare on an empty lobe, reopen cleanly, and prove the spec survived boot:
/// re-declaring the SAME axis is an idempotent no-op, and declaring a DIFFERENT
/// axis errors because the loaded spec already occupies the (single) axis.
#[test]
fn declaration_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path()).unwrap();
        run(&engine, r#"LOBE "mem""#).unwrap();
        let ok = run(&engine, r#"SATELLITE BY kind IN "mem""#).unwrap();
        match ok {
            QueryResult::Ok { message, .. } => {
                assert!(
                    message.contains("kind") && message.contains("mem"),
                    "{message}"
                )
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    // Clean reopen: a fresh boot must load the persisted satellite spec.
    let engine = Engine::open(dir.path()).unwrap();

    // Same axis again → idempotent no-op (proves it decoded to the same spec).
    run(&engine, r#"SATELLITE BY kind IN "mem""#)
        .expect("re-declaring the same axis after reopen must be a no-op");

    // Different axis → rejected, one axis per lobe. This is the discriminator:
    // it can only fire if the spec was loaded at boot.
    let err = run(&engine, r#"SATELLITE BY topic IN "mem""#)
        .expect_err("declaring a different axis after reopen must be rejected");
    assert!(
        err.contains("already has a satellite axis"),
        "expected one-axis-per-lobe error, got: {err}"
    );
}

/// §6: declaring a satellite over a NON-EMPTY lobe is refused — the records
/// already at `sat = 0` would be unreachable by a future bounded query.
#[test]
fn declaration_refused_on_non_empty_lobe() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "notes""#).unwrap();
    run(&engine, r#"PUT {body: "hello", kind: "note"} IN "notes""#).unwrap();

    let err = run(&engine, r#"SATELLITE BY kind IN "notes""#)
        .expect_err("SATELLITE on a non-empty lobe must be refused");
    assert!(
        err.contains("not empty") && err.contains("before the first write"),
        "expected non-empty rejection, got: {err}"
    );

    // The refused declaration left no trace: the records are untouched and a
    // later declaration on a FRESH empty lobe still works.
    assert_eq!(scan(&engine, "notes").len(), 1, "record must be untouched");
    run(&engine, r#"LOBE "fresh""#).unwrap();
    run(&engine, r#"SATELLITE BY kind IN "fresh""#)
        .expect("declaring on a fresh empty lobe must still work");
}

/// A declared satellite is inert in this phase: records written after the
/// declaration are placed identically to today (`sat = 0`) and read back whole.
/// Guards that adding the declaration path did not perturb the write/read path.
#[test]
fn declared_satellite_is_inert() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "events""#).unwrap();
    run(&engine, r#"GRAVITY BY scope IN "events""#).unwrap();
    run(&engine, r#"SATELLITE BY kind IN "events""#).unwrap();

    for i in 0..50 {
        let kind = if i % 2 == 0 { "click" } else { "view" };
        run(
            &engine,
            &format!(r#"PUT {{scope: "s1", kind: "{kind}", n: {i}}} IN "events""#),
        )
        .unwrap();
    }

    // Every record is present and intact — declaration did not drop, misplace,
    // or corrupt any write.
    let records = scan(&engine, "events");
    assert_eq!(records.len(), 50, "all writes must round-trip");
    let clicks = records
        .iter()
        .filter(|r| r.fields.get("kind").and_then(|v| v.as_text()) == Some("click"))
        .count();
    assert_eq!(clicks, 25, "field values must be intact");
}
