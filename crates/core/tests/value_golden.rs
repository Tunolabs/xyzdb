//! Golden fixtures for `Value` / record decoding. The bytes below were captured
//! from the shipping serializer; every test asserts BOTH that serialization still
//! produces these exact bytes (format did not drift) AND that decoding them yields
//! the original value (decode did not drift). The depth-cap change to `Value`'s
//! `Deserialize` (fix(core): cap nesting depth) may ONLY reject over-deep input —
//! it must never alter the decoding of anything here, and these goldens are the
//! guard. V3/V4 record blobs are retired (only V1/V2/V5 are serialized), so the
//! coverage is V1/V2/V5 records + the bare `Value` under postcard and bincode.

// SPDX-License-Identifier: BUSL-1.1
use std::collections::BTreeMap;
use xyzdb_core::field_dict::FieldDict;
use xyzdb_core::lid::LID;
use xyzdb_core::record::{
    Record, deserialize_record, serialize_record, serialize_record_v2, serialize_record_v5,
};
use xyzdb_core::value::Value;

// Captured from the shipping serializer (see the generator in git history).
const VALUE_POSTCARD: &str = "06010701016b0602010e0701017808";
const VALUE_BINCODE: &str = "06000000010000000000000007000000010000000000000001000000000000006b06000000020000000000000001000000070000000000000007000000010000000000000001000000000000007808000000";
const REC_V1: &str = "585901909eb8e8c0e1828589909cb0d080c181820209016200010262790504000102ff0166020000000000000c4001690153016e08066e657374656406010701016b0602010e07010178080174030568656c6c6f027473048080f281838985060376656309030000803f00002040000040c0de01bc03";
const REC_V2: &str = "585902909eb8e8c0e1828589909cb0d080c181820209000001010504000102ff02020000000000000c4003015304080506010701016b0602010e070101780806030568656c6c6f07048080f281838985060809030000803f00002040000040c0de01bc03";
const REC_V5: &str = "585905909eb8e8c0e1828589909cb0d080c181820209000001010504000102ff02020000000000000c4003015304080506010701016b0602010e070101780806030568656c6c6f07048080f281838985060809030000803f00002040000040c0de01bc03";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Nested `Value` at depth ~4 — the recursive shape the cap must decode unchanged.
fn golden_value() -> Value {
    Value::List(vec![Value::Map(BTreeMap::from([(
        "k".to_string(),
        Value::List(vec![
            Value::Int(7),
            Value::Map(BTreeMap::from([("x".to_string(), Value::Null)])),
        ]),
    )]))])
}

fn golden_record() -> Record {
    let mut fields = BTreeMap::new();
    fields.insert("b".to_string(), Value::Bool(true));
    fields.insert("i".to_string(), Value::Int(-42));
    fields.insert("f".to_string(), Value::Float(3.5));
    fields.insert("t".to_string(), Value::Text("hello".to_string()));
    fields.insert("ts".to_string(), Value::Timestamp(1_700_000_000_000_000));
    fields.insert("by".to_string(), Value::Bytes(vec![0, 1, 2, 255]));
    fields.insert("n".to_string(), Value::Null);
    fields.insert("vec".to_string(), Value::Vector(vec![1.0, 2.5, -3.0]));
    fields.insert("nested".to_string(), golden_value());
    Record {
        lid: LID::from_raw(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10),
        lobe_name: "mem".to_string(),
        fields,
        created_at: 111,
        updated_at: 222,
    }
}

#[test]
fn value_postcard_golden() {
    let v = golden_value();
    assert_eq!(
        hex(&postcard::to_allocvec(&v).unwrap()),
        VALUE_POSTCARD,
        "postcard SERIALIZE drifted"
    );
    assert_eq!(
        postcard::from_bytes::<Value>(&unhex(VALUE_POSTCARD)).unwrap(),
        v,
        "postcard DECODE drifted"
    );
}

#[test]
fn value_bincode_golden() {
    let v = golden_value();
    assert_eq!(
        hex(&bincode::serialize(&v).unwrap()),
        VALUE_BINCODE,
        "bincode SERIALIZE drifted"
    );
    assert_eq!(
        bincode::deserialize::<Value>(&unhex(VALUE_BINCODE)).unwrap(),
        v,
        "bincode DECODE drifted"
    );
}

#[test]
fn record_v1_golden() {
    let r = golden_record();
    assert_eq!(hex(&serialize_record(&r)), REC_V1, "V1 SERIALIZE drifted");
    assert_eq!(
        deserialize_record(&unhex(REC_V1), "mem", None)
            .unwrap()
            .fields,
        r.fields,
        "V1 DECODE drifted"
    );
}

#[test]
fn record_v2_golden() {
    let r = golden_record();
    let mut dict = FieldDict::new();
    assert_eq!(
        hex(&serialize_record_v2(&r, &mut dict)),
        REC_V2,
        "V2 SERIALIZE drifted"
    );
    assert_eq!(
        deserialize_record(&unhex(REC_V2), "mem", Some(&dict))
            .unwrap()
            .fields,
        r.fields,
        "V2 DECODE drifted"
    );
}

#[test]
fn record_v5_golden() {
    let r = golden_record();
    let mut dict = FieldDict::new();
    let (blob, _col) = serialize_record_v5(&r, &mut dict, None);
    assert_eq!(hex(&blob), REC_V5, "V5 SERIALIZE drifted");
    assert_eq!(
        deserialize_record(&unhex(REC_V5), "mem", Some(&dict))
            .unwrap()
            .fields,
        r.fields,
        "V5 DECODE drifted"
    );
}
