//! Durability of LINK/SET writes across a restart (regression guards).
//!
//! `PUT`/`DELETE` commit through `turba.batch().commit()` (WAL-backed,
//! fsynced). `SET` and `LINK` used to update the primary spatial record with a
//! raw `Tree::insert`, which writes the ACTIVE MEMTABLE ONLY and bypasses the
//! WAL (turba tree/mod.rs) — so an acked SET/LINK lived only in RAM until the
//! next flush and was lost on a crash before it (a false ack under Durable
//! mode). LINK additionally writes the V2 on-disk format, whose field NAMES
//! are stored as u16 IDs that need the per-lobe field registry to decode; that
//! id→name mapping was persisted only by a deferred, error-swallowing path at
//! COMPACT/Drop and only when a per-lobe dirty flag was set — a flag that was
//! never raised for a new field name added to an already-persisted lobe.
//!
//! The fix routes SET/LINK through the WAL-durable batch path and co-commits
//! the registry entry in the SAME batch as the V2 record (so the mapping is
//! durable iff the record is). These repros TRIGGER the fault (write ->
//! restart -> read); they do not merely assert a property.

use xyzdb_core::record::Record;
use xyzdb_engine::engine::{Engine, QueryResult};

fn run(engine: &Engine, s: &str) -> Result<QueryResult, String> {
    engine.run(s).map_err(|e| format!("{s:?}: {e:?}"))
}

/// SCAN a lobe, returning the records or the read error. A V2 record whose
/// registry mapping is missing surfaces here as an `Err` (bincode fallback) —
/// that is itself the fault, so callers treat `Err` as "record lost".
fn scan(engine: &Engine, lobe: &str) -> Result<Vec<Record>, String> {
    match run(engine, &format!(r#"SCAN "{lobe}""#))? {
        QueryResult::Records(r) => Ok(r),
        QueryResult::PaginatedRecords { records, .. } => Ok(records),
        other => Err(format!("unexpected scan result: {other:?}")),
    }
}

fn field<'a>(rec: &'a Record, name: &str) -> Option<&'a str> {
    rec.fields.get(name).and_then(|v| v.as_text())
}

/// The LINK edge write must reach the WAL, so an acked `_link_<rel>` edge
/// survives a crash that happens before the next flush.
#[test]
fn link_edge_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "items""#).unwrap();
    run(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).unwrap();

    // Target (plain V1) + source rewritten V2 by the LINK clause.
    run(&engine, r#"PUT {id: "B", role: "target"} IN "items""#).unwrap();
    run(
        &engine,
        r#"PUT {id: "A", role: "source"} IN "items" LINK TO "items" WHERE id = "B" AS "owns""#,
    )
    .unwrap();

    // Crash before any flush, skipping the Drop path (SIGKILL) — the turba
    // durability_proptest forget+reopen pattern.
    engine._test_release_dir_lock();
    std::mem::forget(engine);

    let engine = Engine::open(dir.path()).unwrap();
    let records =
        scan(&engine, "items").expect("the acked records must still be readable after restart");

    let a = records
        .iter()
        .find(|r| field(r, "id") == Some("A"))
        .expect("source record A must survive the restart");
    assert!(
        field(a, "_link_owns").is_some(),
        "the acked LINK edge field _link_owns must survive the restart"
    );
}

/// A new field name added to an already-persisted lobe must survive a restart:
/// its id→name mapping is co-committed with the record, not deferred behind a
/// dirty flag. Clean shutdown (no crash) — exercises the COMPACT path.
#[test]
fn new_field_after_first_compact_survives_clean_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path()).unwrap();
        run(&engine, r#"LOBE "items""#).unwrap();
        run(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).unwrap();
        run(&engine, r#"PUT {id: "T", role: "target"} IN "items""#).unwrap();

        // First LINK on the lobe registers id/k/_link_r1 in the registry.
        run(
            &engine,
            r#"PUT {id: "A1", k: "a"} IN "items" LINK TO "items" WHERE id = "T" AS "r1""#,
        )
        .unwrap();
        run(&engine, "COMPACT").unwrap();

        // Second LINK introduces a NEW field name (`extra`) and relation (r2)
        // on the SAME, already-persisted lobe.
        run(
            &engine,
            r#"PUT {id: "A2", k: "a", extra: "keep-me"} IN "items" LINK TO "items" WHERE id = "T" AS "r2""#,
        )
        .unwrap();
        run(&engine, "COMPACT").unwrap();
        // clean shutdown
    }

    let engine = Engine::open(dir.path()).unwrap();
    let records = scan(&engine, "items").expect("scan after clean restart");
    let a2 = records
        .iter()
        .find(|r| field(r, "id") == Some("A2"))
        .expect("record A2 must survive the clean restart");

    assert_eq!(
        field(a2, "extra"),
        Some("keep-me"),
        "a new field added after the lobe's first persist must survive a clean restart"
    );
    assert!(
        field(a2, "_link_r2").is_some(),
        "the r2 edge field must survive a clean restart"
    );
}

/// A `SET` update must reach the WAL, so an acked field change survives a crash
/// before the next flush.
#[test]
fn set_update_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "items""#).unwrap();
    run(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).unwrap();
    run(&engine, r#"PUT {id: "A", status: "old"} IN "items""#).unwrap();
    run(&engine, r#"SET "items" status = "new" WHERE id = "A""#).unwrap();

    engine._test_release_dir_lock();
    std::mem::forget(engine); // crash before any flush

    let engine = Engine::open(dir.path()).unwrap();
    let records = scan(&engine, "items").expect("scan after restart");
    let a = records
        .iter()
        .find(|r| field(r, "id") == Some("A"))
        .expect("record A must survive the restart");
    assert_eq!(
        field(a, "status"),
        Some("new"),
        "the acked SET must survive a crash before flush"
    );
}

/// A `PUT ... ON CONFLICT UPDATE` (upsert) update must reach the WAL, so the
/// merged fields survive a crash before the next flush.
#[test]
fn upsert_update_survives_crash() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    run(&engine, r#"LOBE "items""#).unwrap();
    run(&engine, r#"ANCHOR "id" UNIQUE IN "items""#).unwrap();
    run(&engine, r#"PUT {id: "A", status: "old"} IN "items""#).unwrap();
    // Anchor collision + ON CONFLICT -> execute_upsert (the update branch).
    run(
        &engine,
        r#"PUT {id: "A", status: "new"} IN "items" ON CONFLICT UPDATE"#,
    )
    .unwrap();

    engine._test_release_dir_lock();
    std::mem::forget(engine); // crash before any flush

    let engine = Engine::open(dir.path()).unwrap();
    let records = scan(&engine, "items").expect("scan after restart");
    let a = records
        .iter()
        .find(|r| field(r, "id") == Some("A"))
        .expect("record A must survive the restart");
    assert_eq!(
        field(a, "status"),
        Some("new"),
        "the acked upsert update must survive a crash before flush"
    );
}
