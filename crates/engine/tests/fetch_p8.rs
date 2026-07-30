//! Teeth for xyTalk v1 P8: `FETCH "a","b" WHERE … [AS {..}]` reads N co-located
//! lobes in one call and returns one record with a named section per lobe.
//!
//! The load-bearing properties: each section is byte-for-byte what the same
//! `SCAN … WHERE` on that lobe returns (packaging, not composition), and it is a
//! single call — one result, all sections — versus N separate scans.

use std::collections::BTreeMap;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("query failed: {s}\n  err: {e:?}"))
}

/// The single envelope record's sections: name -> its list of field-maps.
fn fetch_sections(engine: &Engine, q: &str) -> BTreeMap<String, Vec<BTreeMap<String, Value>>> {
    let recs = match exec(engine, q) {
        QueryResult::Records(rs) => rs,
        other => panic!("FETCH must return Records, got {other:?}"),
    };
    assert_eq!(recs.len(), 1, "FETCH returns exactly one envelope record");
    let mut out = BTreeMap::new();
    for (name, val) in &recs[0].fields {
        let rows = match val {
            Value::List(items) => items
                .iter()
                .map(|v| match v {
                    Value::Map(m) => m.clone(),
                    other => panic!("section entry must be a Map, got {other:?}"),
                })
                .collect(),
            other => panic!("section must be a List, got {other:?}"),
        };
        out.insert(name.clone(), rows);
    }
    out
}

/// Records of a plain scan, as sorted field-maps (order-independent compare).
fn scan_maps(engine: &Engine, q: &str) -> Vec<BTreeMap<String, Value>> {
    let mut v: Vec<BTreeMap<String, Value>> = match exec(engine, q) {
        QueryResult::Records(rs) => rs.into_iter().map(|r| r.fields).collect(),
        other => panic!("expected Records, got {other:?}"),
    };
    v.sort_by_key(|m| format!("{m:?}"));
    v
}

fn sorted(mut v: Vec<BTreeMap<String, Value>>) -> Vec<BTreeMap<String, Value>> {
    v.sort_by_key(|m| format!("{m:?}"));
    v
}

fn seed(engine: &Engine) {
    exec(engine, r#"LOBE "clientes""#);
    exec(engine, r#"LOBE "creditos""#);
    exec(
        engine,
        r#"PUT {*rfc:"X", _type:"Cliente", name:"Acme"} IN "clientes""#,
    );
    exec(
        engine,
        r#"PUT {*rfc:"Y", _type:"Cliente", name:"Other"} IN "clientes""#,
    );
    exec(
        engine,
        r#"PUT {*rfc:"X", _type:"Credit", monto:100} IN "creditos""#,
    );
    exec(
        engine,
        r#"PUT {*rfc:"X", _type:"Credit", monto:200} IN "creditos""#,
    );
    exec(
        engine,
        r#"PUT {*rfc:"Y", _type:"Credit", monto:999} IN "creditos""#,
    );
}

/// Each section equals the same `SCAN WHERE` on that lobe — same content — and
/// it arrives as one call (one envelope with both sections).
#[test]
fn fetch_sections_equal_separate_scans() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e);

    let sections = fetch_sections(&e, r#"FETCH "clientes", "creditos" WHERE rfc = "X""#);

    // Single roundtrip: exactly the two requested sections, default-named by lobe.
    let keys: Vec<&String> = sections.keys().collect();
    assert_eq!(
        keys,
        vec!["clientes", "creditos"],
        "sections default-named by lobe"
    );

    // Equivalence: each section == the standalone scan on that lobe.
    assert_eq!(
        sorted(sections["clientes"].clone()),
        scan_maps(&e, r#"SCAN "clientes" WHERE rfc = "X" LIMIT 100"#),
        "clientes section must equal the standalone scan"
    );
    assert_eq!(
        sorted(sections["creditos"].clone()),
        scan_maps(&e, r#"SCAN "creditos" WHERE rfc = "X" LIMIT 100"#),
        "creditos section must equal the standalone scan"
    );
    // X has 1 cliente + 2 credits; Y's records must not leak in.
    assert_eq!(sections["clientes"].len(), 1);
    assert_eq!(sections["creditos"].len(), 2);
}

/// `AS {..}` renames the sections positionally.
#[test]
fn fetch_as_renames_sections() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e);

    let sections = fetch_sections(
        &e,
        r#"FETCH "clientes", "creditos" WHERE rfc = "X" AS {cliente, creditos}"#,
    );
    let keys: Vec<&String> = sections.keys().collect();
    assert_eq!(keys, vec!["cliente", "creditos"], "AS renames positionally");
    assert_eq!(sections["cliente"].len(), 1);
}

/// AS with the wrong number of names, and a WHERE-less FETCH, both error.
#[test]
fn fetch_rejects_bad_as_and_missing_where() {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path()).unwrap();
    seed(&e);

    let bad_as = e
        .run(r#"FETCH "clientes", "creditos" WHERE rfc = "X" AS {only_one}"#)
        .unwrap_err();
    assert!(
        format!("{bad_as:?}").contains("one section name per lobe")
            || format!("{bad_as:?}").contains("name(s) for"),
        "AS count mismatch must error: {bad_as:?}"
    );

    let no_where = e.run(r#"FETCH "clientes", "creditos""#).unwrap_err();
    assert!(
        format!("{no_where:?}").contains("WHERE"),
        "WHERE-less FETCH must error teaching WHERE: {no_where:?}"
    );
}
