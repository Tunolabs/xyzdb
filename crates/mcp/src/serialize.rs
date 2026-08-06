//! `QueryResult` → JSON serialization for the `query` tool.
//!
//! Pillar 2 of v0.2.6 implementation phase. This module produces the
//! same JSON shape as `xyzdb-server::json_response::serialize_json` so
//! the `query` tool returns identical output across `--embed` and
//! `--connect` modes (the latter passes through the server's JSON
//! verbatim).
//!
//! Why a parallel implementation rather than a path-dep on
//! `xyzdb-server`: the server crate pulls the full TCP server, JSON
//! response paths, V3 bulk frames, etc. — far more than we need and
//! it inflates the `xyzdb-mcp` binary. ~80 LOC of mapping is the
//! cheaper trade.
//!
//! Drift risk: if `xyzdb-server::json_response` changes shape (e.g. a
//! new field on `PaginatedRecords`), this module must follow. The
//! existing fields are stable (paginated cursor shipped in v0.2.5.1)
//! so drift is bounded.

// SPDX-License-Identifier: BUSL-1.1
use serde_json::{Value as JsonValue, json};
use xyzdb_core::lid::LID;
use xyzdb_core::record::Record;
use xyzdb_core::result::QueryResult;
use xyzdb_core::value::Value;

/// Serialize a `QueryResult` as a JSON value with the same shape the
/// xyzdb-server emits over V2 with `FORMAT_JSON`. Includes a top-level
/// `time_ms` field for parity with the server output.
pub fn query_result_to_json(result: &QueryResult, elapsed_ms: f64) -> JsonValue {
    match result {
        QueryResult::Ok { lid, message } => {
            let mut obj = json!({
                "status": "ok",
                "message": message,
            });
            if let Some(lid) = lid {
                obj["lid"] = json!(lid.to_string());
            }
            obj
        }
        QueryResult::BatchOk {
            count,
            first_lid,
            last_lid,
        } => json!({
            "status": "ok",
            "count": count,
            "first_lid": first_lid.to_string(),
            "last_lid": last_lid.to_string(),
            "message": format!("{count} records inserted"),
        }),
        QueryResult::Records(records) => build_flat_records(records, elapsed_ms),
        QueryResult::Aggregation(agg) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in agg {
                obj.insert(k.clone(), value_to_json(v));
            }
            json!({
                "status": "ok",
                "aggregation": JsonValue::Object(obj),
                "time_ms": elapsed_ms,
            })
        }
        QueryResult::Info(lines) => json!({
            "status": "ok",
            "info": lines,
        }),
        QueryResult::GroupedAggregation(groups) => {
            let group_results: Vec<JsonValue> = groups
                .iter()
                .map(|group| {
                    let mut obj = serde_json::Map::new();
                    for (k, v) in group {
                        obj.insert(k.clone(), value_to_json(v));
                    }
                    JsonValue::Object(obj)
                })
                .collect();
            json!({
                "status": "ok",
                "groups": group_results,
                "time_ms": elapsed_ms,
            })
        }
        QueryResult::PaginatedRecords {
            records,
            cursor,
            has_more,
            budget_stop,
        } => {
            let json_records: Vec<JsonValue> = records.iter().map(record_to_json).collect();
            let mut obj = serde_json::Map::new();
            obj.insert("status".into(), json!("ok"));
            obj.insert("records".into(), JsonValue::Array(json_records));
            obj.insert("count".into(), json!(records.len()));
            obj.insert("has_more".into(), json!(has_more));
            // M2.3 budget-stop fact — present only on a NEAREST truncated by the
            // airbag; absent otherwise (additive, existing frames unchanged).
            if let Some(bs) = budget_stop {
                obj.insert(
                    "budget_stop".into(),
                    json!({
                        "examined": bs.examined,
                        "candidates": bs.candidates,
                        "found": bs.found,
                        "strategy": bs.strategy,
                    }),
                );
            }
            if let Some(token) = cursor {
                obj.insert("cursor".into(), json!(token));
            } else {
                obj.insert("cursor".into(), JsonValue::Null);
            }
            obj.insert("time_ms".into(), json!(format!("{:.3}", elapsed_ms)));
            JsonValue::Object(obj)
        }
    }
}

/// Flat records response shape (FIND, SCAN). Spike-grade: ignores
/// the is_pull/nested distinction the server makes for PULL queries.
/// Pillar 6 / UAT may revisit if agents need nested PULL output.
fn build_flat_records(records: &[Record], elapsed_ms: f64) -> JsonValue {
    let json_records: Vec<JsonValue> = records.iter().map(record_to_json).collect();
    json!({
        "status": "ok",
        "records": json_records,
        "count": records.len(),
        "time_ms": elapsed_ms,
    })
}

/// Serialize a single `Record` as JSON. Includes `_lid`, `_type`,
/// `_created_at`, `_updated_at` plus user fields.
pub fn record_to_json(r: &Record) -> JsonValue {
    let mut obj = serde_json::Map::new();
    obj.insert("_lid".into(), json!(r.lid.to_string()));
    obj.insert("_type".into(), json!(r.lobe_name));
    obj.insert("_created_at".into(), json!(r.created_at));
    obj.insert("_updated_at".into(), json!(r.updated_at));
    for (k, v) in &r.fields {
        obj.insert(k.clone(), value_to_json(v));
    }
    JsonValue::Object(obj)
}

/// Map `xyzdb_core::Value` to a `serde_json::Value`.
pub fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Text(s) => json!(s),
        Value::Timestamp(ts) => json!(ts),
        Value::Bytes(b) => json!(format!("<{} bytes>", b.len())),
        Value::List(items) => JsonValue::Array(items.iter().map(value_to_json).collect()),
        Value::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in m {
                obj.insert(k.clone(), value_to_json(v));
            }
            JsonValue::Object(obj)
        }
        Value::Vector(packed) => {
            JsonValue::Array(packed.iter().map(|x| json!(*x as f64)).collect())
        }
    }
}

// `LID` import kept for future use (PULL nested response in Pillar 6+).
#[allow(dead_code)]
fn _placeholder_lid_use() -> Option<LID> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use xyzdb_core::lid::LID;
    use xyzdb_core::record::Record;
    use xyzdb_core::result::BudgetStop;

    fn sample_record() -> Record {
        let mut fields = BTreeMap::new();
        fields.insert("rfc".into(), Value::Text("ACME-001".into()));
        fields.insert("monto".into(), Value::Float(50000.0));
        fields.insert("active".into(), Value::Bool(true));
        Record {
            lid: LID::new(1u16),
            lobe_name: "clientes".into(),
            fields,
            created_at: 1_700_000_000_000_000,
            updated_at: 1_700_000_001_000_000,
        }
    }

    #[test]
    fn ok_variant_basic() {
        let qr = QueryResult::Ok {
            lid: Some(LID::new(1u16)),
            message: "lobe created".into(),
        };
        let v = query_result_to_json(&qr, 1.5);
        assert_eq!(v["status"], "ok");
        assert!(v["lid"].is_string());
        assert_eq!(v["message"], "lobe created");
    }

    #[test]
    fn ok_variant_no_lid() {
        let qr = QueryResult::Ok {
            lid: None,
            message: "anchor declared".into(),
        };
        let v = query_result_to_json(&qr, 0.5);
        assert!(v.get("lid").is_none());
    }

    #[test]
    fn records_flat_shape() {
        let qr = QueryResult::Records(vec![sample_record()]);
        let v = query_result_to_json(&qr, 12.3);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 1);
        let rec = &v["records"][0];
        assert_eq!(rec["_type"], "clientes");
        assert_eq!(rec["rfc"], "ACME-001");
        assert_eq!(rec["monto"], 50000.0);
        assert_eq!(rec["active"], true);
    }

    /// The two hand-written serializers (server `json_response` and
    /// `xyzdb-mcp::serialize`) must emit EXACTLY the fields of `BudgetStop`.
    ///
    /// They are parallel implementations of one response shape — a deliberate
    /// trade (the MCP binary must not link the whole TCP server) whose known cost
    /// is drift. That cost was paid: `strategy` shipped on the wire and was missing
    /// from MCP, so an agent received a partial with no way to tell a PREFIX of the
    /// answer from a sample of a key region. Adding the field fixes that instance;
    /// this test closes the class.
    ///
    /// The oracle is the struct itself, via serde — not a list typed out here,
    /// which would drift in the same way. Add a field to `BudgetStop` and this test
    /// fails in BOTH crates until both serializers follow.
    #[test]
    fn budget_stop_emits_exactly_the_struct_fields() {
        let bs = BudgetStop {
            examined: 1,
            candidates: 2,
            found: 3,
            strategy: xyzdb_core::result::ScanStrategy::ScoreOrder,
        };
        let canonical: std::collections::BTreeSet<String> =
            match serde_json::to_value(&bs).expect("serialize BudgetStop") {
                serde_json::Value::Object(m) => m.keys().cloned().collect(),
                other => panic!("BudgetStop must serialize to an object, got {other:?}"),
            };
        let qr = QueryResult::PaginatedRecords {
            records: vec![],
            cursor: None,
            has_more: true,
            budget_stop: Some(bs),
        };
        let emitted: std::collections::BTreeSet<String> = match &query_result_to_json(&qr, 1.0)["budget_stop"]
        {
            JsonValue::Object(m) => m.keys().cloned().collect(),
            other => panic!("budget_stop must be an object, got {other:?}"),
        };
        assert_eq!(
            emitted, canonical,
            "MCP JSON drifted from BudgetStop's fields — the parallel serializer \
             fell behind the wire shape again"
        );
    }

    #[test]
    fn paginated_shape_includes_cursor() {
        let qr = QueryResult::PaginatedRecords {
            records: vec![sample_record()],
            cursor: Some("AQEAAQ_DUMMY".into()),
            has_more: true,
            budget_stop: None,
        };
        let v = query_result_to_json(&qr, 9.9);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["has_more"], true);
        assert_eq!(v["cursor"], "AQEAAQ_DUMMY");
        assert_eq!(v["count"], 1);
    }

    #[test]
    fn paginated_shape_null_cursor_when_absent() {
        let qr = QueryResult::PaginatedRecords {
            records: vec![sample_record()],
            cursor: None,
            has_more: false,
            budget_stop: None,
        };
        let v = query_result_to_json(&qr, 1.0);
        assert_eq!(v["cursor"], JsonValue::Null);
        assert_eq!(v["has_more"], false);
    }

    #[test]
    fn aggregation_shape() {
        let mut agg = BTreeMap::new();
        agg.insert("count".into(), Value::Int(42));
        agg.insert("sum_monto".into(), Value::Float(123456.78));
        let qr = QueryResult::Aggregation(agg);
        let v = query_result_to_json(&qr, 0.4);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["aggregation"]["count"], 42);
        assert_eq!(v["aggregation"]["sum_monto"], 123456.78);
    }

    #[test]
    fn info_shape() {
        let qr = QueryResult::Info(vec!["lobe1".into(), "lobe2".into()]);
        let v = query_result_to_json(&qr, 0.1);
        assert_eq!(v["info"], json!(["lobe1", "lobe2"]));
    }

    #[test]
    fn batch_ok_shape() {
        let qr = QueryResult::BatchOk {
            count: 1500,
            first_lid: LID::new(1u16),
            last_lid: LID::new(1u16),
        };
        let v = query_result_to_json(&qr, 234.5);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["count"], 1500);
        assert!(v["first_lid"].is_string());
        assert!(v["last_lid"].is_string());
        assert!(v["message"].as_str().unwrap().contains("1500"));
    }

    #[test]
    fn value_list_serializes_recursively() {
        let v = Value::List(vec![
            Value::Int(1),
            Value::Text("two".into()),
            Value::Bool(false),
        ]);
        let j = value_to_json(&v);
        assert_eq!(j[0], 1);
        assert_eq!(j[1], "two");
        assert_eq!(j[2], false);
    }

    #[test]
    fn value_map_preserves_keys() {
        let mut m = BTreeMap::new();
        m.insert("k1".into(), Value::Int(1));
        m.insert("k2".into(), Value::Text("v".into()));
        let v = Value::Map(m);
        let j = value_to_json(&v);
        assert_eq!(j["k1"], 1);
        assert_eq!(j["k2"], "v");
    }
}
