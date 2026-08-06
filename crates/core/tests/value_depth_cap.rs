//! Regression for the postcard `Value` recursion overflow: a deeply nested
//! payload (≤16 MiB frame) must be REJECTED with a clean, typed error, never
//! decoded and never allowed to overflow the stack (which pre-fix aborted the
//! whole process via SIGABRT). Covers both consumers of the decode path — the
//! wire (a bare postcard `Value`) and disk (a record blob whose field holds an
//! over-deep `Value`).

// SPDX-License-Identifier: BUSL-1.1
use std::collections::BTreeMap;
use xyzdb_core::lid::LID;
use xyzdb_core::record::{Record, deserialize_record, serialize_record};
use xyzdb_core::value::Value;

/// `n` nested `List(vec![...])` postcard bytes built ITERATIVELY (so the encode
/// side does not recurse): each level = [variant 6 = List][len 1][element];
/// innermost leaf = Null (variant 8). `n` nested lists put the leaf at depth
/// `n + 1` (the top-level `Value` is depth 1).
fn nested_list_postcard(n: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(n * 2 + 1);
    for _ in 0..n {
        b.push(6u8);
        b.push(1u8);
    }
    b.push(8u8);
    b
}

#[test]
fn wire_attack_size_is_rejected_not_crashed() {
    // The exact shape that overflowed the worker stack pre-fix: ~2M levels in
    // ~4 MiB. Post-fix it returns a clean Err in microseconds (the cap trips at
    // depth 33, long before any stack pressure).
    let bytes = nested_list_postcard(2_000_000);
    let r = postcard::from_bytes::<Value>(&bytes);
    assert!(
        r.is_err(),
        "a deeply nested postcard Value must be rejected"
    );
}

#[test]
fn depth_boundary_is_exactly_the_cap() {
    // 31 nested lists → leaf at depth 32 == MAX_DECODE_DEPTH → decodes.
    assert!(
        postcard::from_bytes::<Value>(&nested_list_postcard(31)).is_ok(),
        "depth 32 (== cap) must still decode"
    );
    // 32 nested lists → leaf at depth 33 > cap → rejected.
    assert!(
        postcard::from_bytes::<Value>(&nested_list_postcard(32)).is_err(),
        "depth 33 (> cap) must be rejected"
    );
}

#[test]
fn disk_read_path_rejects_over_deep_value_not_crashes() {
    // A record whose field holds an over-deep Value (40 levels encodes fine —
    // only decoding is capped). Reading it back must Err, not abort.
    let mut deep = Value::Null;
    for _ in 0..40 {
        deep = Value::List(vec![deep]);
    }
    let mut fields = BTreeMap::new();
    fields.insert("k".to_string(), deep);
    let r = Record {
        lid: LID::from_raw(1),
        lobe_name: "m".to_string(),
        fields,
        created_at: 0,
        updated_at: 0,
    };
    let blob = serialize_record(&r); // V1 encode is fine at depth 40
    let got = deserialize_record(&blob, "m", None);
    assert!(
        got.is_err(),
        "a record carrying an over-deep Value must be rejected on read, not crash"
    );
}
