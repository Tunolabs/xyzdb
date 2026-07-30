//! S1: bound-parameter (`$name`) substitution and the anti-injection guarantee.
//!
//! A `$param` is replaced by its bound value AFTER parsing, so untrusted text
//! never enters the query string as syntax. An unbound `$param` is a hard error,
//! never silently treated as a literal.

use std::collections::HashMap;
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn count(qr: QueryResult) -> usize {
    match qr {
        QueryResult::Records(r) => r.len(),
        other => panic!("expected Records, got {other:?}"),
    }
}

fn params(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn param_binds_in_where() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "m""#).unwrap();
    engine.run(r#"PUT {name: "alice"} IN "m""#).unwrap();
    engine.run(r#"PUT {name: "bob"} IN "m""#).unwrap();

    let p = params(&[("who", Value::Text("alice".into()))]);
    let r = engine
        .run_with_params(r#"SCAN "m" WHERE name = $who"#, &p)
        .unwrap();
    assert_eq!(count(r), 1, "bound param matches exactly one record");
}

#[test]
fn unbound_param_is_a_hard_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "m""#).unwrap();
    // run() has no bindings: $who must error, never be treated as a literal.
    let err = engine.run(r#"SCAN "m" WHERE name = $who"#).unwrap_err();
    assert!(
        format!("{err:?}").contains("unbound parameter"),
        "expected unbound-parameter error, got {err:?}"
    );
}

#[test]
fn param_value_with_injection_chars_stays_literal() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "m""#).unwrap();
    engine.run(r#"PUT {name: "alice"} IN "m""#).unwrap();
    engine.run(r#"PUT {name: "bob"} IN "m""#).unwrap();

    // A value crafted to break out of the string and inject a DELETE. Passed as
    // a bound param it is a plain Text value: matches no record, executes nothing.
    let p = params(&[("x", Value::Text(r#"alice" | DELETE"#.into()))]);
    let r = engine
        .run_with_params(r#"SCAN "m" WHERE name = $x"#, &p)
        .unwrap();
    assert_eq!(count(r), 0, "injection string matches literally → nothing");

    // Both records survive: no DELETE was executed.
    let all = engine.run(r#"SCAN "m""#).unwrap();
    assert_eq!(count(all), 2, "no record deleted by the injection attempt");
}

#[test]
fn param_binds_in_put() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "m""#).unwrap();

    let p = params(&[("v", Value::Text("carol".into()))]);
    engine
        .run_with_params(r#"PUT {name: $v} IN "m""#, &p)
        .unwrap();

    let r = engine.run(r#"SCAN "m" WHERE name = "carol""#).unwrap();
    assert_eq!(count(r), 1, "PUT bound a param and the record is queryable");
}

#[test]
fn unsupported_param_type_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "m""#).unwrap();
    // Timestamp/Bytes are not bindable yet → clean error, never a panic.
    let p = params(&[("t", Value::Timestamp(0))]);
    let err = engine
        .run_with_params(r#"SCAN "m" WHERE at = $t"#, &p)
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("not bindable"),
        "expected not-bindable error, got {err:?}"
    );
}
