//! V5 write-path split: a PUT into a lobe with a declared searchable vector
//! whose record actually carries that `Value::Vector` stores the V5 record
//! format (blob WITHOUT the vector) plus a separate column entry in the
//! `vectors` keyspace, keyed by the same spatial key, carrying the vector and
//! its stored squared norm. A lobe with no vector declaration keeps the V1
//! format unchanged.

use xyzdb_core::record::{format_version, read_vector_prefix_raw_norm};
use xyzdb_core::value::Value;
use xyzdb_engine::engine::{Engine, QueryResult};

fn exec(engine: &Engine, s: &str) -> QueryResult {
    engine
        .run(s)
        .unwrap_or_else(|e| panic!("exec {s:?}: {e:?}"))
}

/// A 64-element float list literal — at/above `VECTOR_F32_MIN_DIMS`, so the
/// executor packs it into a `Value::Vector`. The first two coords are 1.0, 0.0;
/// the rest are 0.0, so the round-tripped vector is checkable by inspection.
fn emb_literal() -> (String, Vec<f32>) {
    let mut floats = vec![0.0f32; 64];
    floats[0] = 1.0;
    let parts: Vec<String> = floats.iter().map(|f| format!("{f:.1}")).collect();
    (format!("[{}]", parts.join(", ")), floats)
}

/// Resolve a LID's spatial key via the identity keyspace (LID → spatial key).
fn spatial_key_for(engine: &Engine, lid: &xyzdb_core::lid::LID) -> Vec<u8> {
    engine
        .turba()
        .identity
        .get(&lid.to_bytes())
        .expect("identity get")
        .expect("LID present in identity")
}

/// Resolve a LID's stored spatial record bytes via the public Turba keyspaces:
/// identity (LID → spatial key) then spatial (key → record blob).
fn record_bytes_for(engine: &Engine, lid: &xyzdb_core::lid::LID) -> Vec<u8> {
    let sk = spatial_key_for(engine, lid);
    engine
        .turba()
        .spatial
        .get(&sk)
        .expect("spatial get")
        .expect("record present in spatial")
}

/// Resolve a LID's V5 vector column bytes from the `vectors` keyspace, keyed by
/// the same spatial key as the record blob.
fn column_bytes_for(engine: &Engine, lid: &xyzdb_core::lid::LID) -> Option<Vec<u8>> {
    let sk = spatial_key_for(engine, lid);
    engine.turba().vectors.get(&sk).expect("vectors get")
}

#[test]
fn put_with_declared_vector_stores_v5_with_column_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    exec(&engine, r#"LOBE "m""#);
    exec(&engine, r#"VECTOR emb IN "m""#);

    let (lit, expected_vec) = emb_literal();
    let put = exec(
        &engine,
        &format!(r#"PUT {{id: "a", note: "hi", emb: {lit}}} IN "m""#),
    );
    let lid = match put {
        QueryResult::Ok { lid: Some(l), .. } => l,
        other => panic!("expected Ok with a LID, got {other:?}"),
    };

    // (a) Reconstructed record equals what went in: the hoisted vector is back
    // in `fields` under its name, indistinguishable from a non-hoisted record.
    let qr = exec(&engine, r#"FIND "m" WHERE id = "a""#);
    let recs = match qr {
        QueryResult::Records(r) => r,
        other => panic!("expected Records, got {other:?}"),
    };
    assert_eq!(recs.len(), 1, "exactly one record back");
    let rec = &recs[0];
    match rec.fields.get("emb") {
        Some(Value::Vector(v)) => assert_eq!(v, &expected_vec, "vector survives round-trip"),
        other => panic!("emb not a Vector after round-trip: {other:?}"),
    }
    assert_eq!(rec.fields.get("id"), Some(&Value::Text("a".into())));
    assert_eq!(rec.fields.get("note"), Some(&Value::Text("hi".into())));

    // (b) The record blob is V5 and carries NO vector prefix — the vector was
    // hoisted out into the column. The blob is the RAM-light entry NEAREST never
    // touches while scanning.
    let bytes = record_bytes_for(&engine, &lid);
    assert_eq!(format_version(&bytes), 5, "stored record must be V5");
    assert!(
        read_vector_prefix_raw_norm(&bytes).is_none(),
        "V5 blob holds no vector prefix — the vector lives in the column"
    );

    // (c) The column entry (same spatial key, `vectors` keyspace) exposes the
    // vector + its stored norm via the existing prefix reader. The norm is the
    // raw sum of squares in index order, bit-for-bit.
    let column = column_bytes_for(&engine, &lid).expect("V5 PUT writes a vector column");
    let (_lid, _fid, fbytes, norm_sq) =
        read_vector_prefix_raw_norm(&column).expect("column exposes the vector prefix");
    let floats: Vec<f32> = fbytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(floats, expected_vec, "column vector equals input");
    let expected_norm: f64 = expected_vec.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    assert_eq!(
        norm_sq.expect("column stores the squared norm").to_bits(),
        expected_norm.to_bits(),
        "stored ‖v‖² is the raw sum of squares, bit-for-bit"
    );
}

#[test]
fn put_into_lobe_without_vector_declaration_stays_v1() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();

    // No VECTOR declaration: fintech-style lobe, records stay V1 (golden frozen).
    exec(&engine, r#"LOBE "fin""#);
    let (lit, _) = emb_literal();
    let put = exec(
        &engine,
        &format!(r#"PUT {{id: "b", amount: 100, emb: {lit}}} IN "fin""#),
    );
    let lid = match put {
        QueryResult::Ok { lid: Some(l), .. } => l,
        other => panic!("expected Ok with a LID, got {other:?}"),
    };

    let bytes = record_bytes_for(&engine, &lid);
    assert_eq!(
        format_version(&bytes),
        1,
        "no declared vector → record must stay V1 (non-vector path unchanged)"
    );
    assert!(
        read_vector_prefix_raw_norm(&bytes).is_none(),
        "V1 record exposes no prefix vector"
    );
    assert!(
        column_bytes_for(&engine, &lid).is_none(),
        "no declared vector → no column entry in the vectors keyspace"
    );
}
