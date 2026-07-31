use serde_json::{Value as JsonValue, json};
use std::time::Duration;
use xyzdb_core::lid::LID;
use xyzdb_core::record::Record;
use xyzdb_core::result::QueryResult;
use xyzdb_core::value::Value;

/// Serialize a QueryResult as JSON bytes.
pub fn serialize_json(
    result: &QueryResult,
    elapsed: Duration,
    is_pull: bool,
    root_lid: Option<&LID>,
) -> Vec<u8> {
    let json = match result {
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
        QueryResult::Records(records) => {
            if is_pull && !records.is_empty() {
                if let Some(lid) = root_lid {
                    build_nested_response(lid, records, elapsed)
                } else {
                    build_flat_response(records, elapsed)
                }
            } else {
                build_flat_response(records, elapsed)
            }
        }
        QueryResult::Aggregation(agg) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in agg {
                obj.insert(k.clone(), value_to_json(v));
            }
            json!({
                "status": "ok",
                "aggregation": JsonValue::Object(obj),
                "time_ms": elapsed.as_secs_f64() * 1000.0,
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
                "time_ms": elapsed.as_secs_f64() * 1000.0,
            })
        }
        QueryResult::PaginatedRecords {
            records,
            cursor,
            has_more,
            budget_stop,
        } => {
            // v0.2.5.1: paginated SCAN — same record schema as the flat
            // `records` shape, with extra `cursor` (opaque token, present
            // only when has_more=true) and `has_more` boolean. Clients
            // ready for pagination key off `has_more`; older clients that
            // expect `Records` see a clearly different status payload and
            // fail with "unknown field" rather than silently mis-paginate.
            let json_records: Vec<JsonValue> = records.iter().map(record_to_json).collect();
            let mut obj = serde_json::Map::new();
            obj.insert("status".into(), json!("ok"));
            obj.insert("records".into(), JsonValue::Array(json_records));
            obj.insert("count".into(), json!(records.len()));
            obj.insert("has_more".into(), json!(has_more));
            // M2.3: the budget-stop fact, emitted ONLY when present (a NEAREST
            // whose hydration hit the airbag). Absent otherwise, so ordinary
            // pagination frames stay byte-identical — additive, PATCH-safe.
            if let Some(bs) = budget_stop {
                obj.insert(
                    "budget_stop".into(),
                    json!({
                        "examined": bs.examined,
                        "candidates": bs.candidates,
                        "found": bs.found,
                        // Says WHAT KIND of partial this is: a prefix (score_order)
                        // or the best of a key region (key_order, where the unwalked
                        // part may hold better rows). Without it the documented
                        // "extrapolate the pass rate" reading is false for one order.
                        "strategy": bs.strategy,
                    }),
                );
            }
            if let Some(token) = cursor {
                obj.insert("cursor".into(), json!(token));
            } else {
                obj.insert("cursor".into(), JsonValue::Null);
            }
            obj.insert(
                "time_ms".into(),
                json!(format!("{:.3}", elapsed.as_secs_f64() * 1000.0)),
            );
            JsonValue::Object(obj)
        }
    };

    serde_json::to_vec(&json).unwrap_or_else(|_| b"{}".to_vec())
}

/// Serialize an error as JSON bytes.
pub fn serialize_json_error(error: &str) -> Vec<u8> {
    let code = error_code(error);
    let json = json!({
        "status": "error",
        "error": error,
        "code": code,
    });
    serde_json::to_vec(&json).unwrap_or_else(|_| b"{}".to_vec())
}

// ─── Flat response (FIND, SCAN) ─────────────────────────────────────────────

fn build_flat_response(records: &[Record], elapsed: Duration) -> JsonValue {
    let json_records: Vec<JsonValue> = records.iter().map(record_to_json).collect();
    json!({
        "status": "ok",
        "records": json_records,
        "count": records.len(),
        "time_ms": format!("{:.3}", elapsed.as_secs_f64() * 1000.0),
    })
}

// ─── Nested response (PULL) ─────────────────────────────────────────────────

fn build_nested_response(root_lid: &LID, records: &[Record], elapsed: Duration) -> JsonValue {
    let root_lid_str = root_lid.to_string();

    // Find the root record
    let root = records.iter().find(|r| r.lid.to_string() == root_lid_str);

    let root_json = match root {
        Some(r) => {
            let mut node = build_nested_node(r, records);
            // Fallback: any records not claimed by the tree go under root._related
            let claimed = collect_claimed_lids(&node);
            let unclaimed: Vec<&Record> = records
                .iter()
                .filter(|rec| rec.lid != r.lid && !claimed.contains(&rec.lid.to_string()))
                .collect();
            if !unclaimed.is_empty() {
                let existing = node
                    .get("_related")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut all_related = existing;
                for rec in unclaimed {
                    all_related.push(record_to_json(rec));
                }
                node["_related"] = json!(all_related);
            }
            node
        }
        None => return build_flat_response(records, elapsed),
    };

    json!({
        "status": "ok",
        "root": root_json,
        "total_records": records.len(),
        "time_ms": format!("{:.3}", elapsed.as_secs_f64() * 1000.0),
    })
}

/// Collect all LIDs that appear in the nested tree (for fallback grouping).
fn collect_claimed_lids(node: &JsonValue) -> Vec<String> {
    let mut lids = Vec::new();
    if let Some(lid) = node.get("lid").and_then(|v| v.as_str()) {
        lids.push(lid.to_string());
    }
    if let Some(related) = node.get("_related").and_then(|v| v.as_array()) {
        for child in related {
            lids.extend(collect_claimed_lids(child));
        }
    }
    lids
}

/// Build a nested JSON node from a record.
/// Children are records whose _link_* field points to this record's LID.
fn build_nested_node(record: &Record, all_records: &[Record]) -> JsonValue {
    let my_lid = record.lid.to_string();
    let mut obj = record_to_json(record);

    // Find children: records whose _link_* value matches this record's LID
    let children: Vec<&Record> = all_records
        .iter()
        .filter(|r| {
            r.lid != record.lid
                && r.fields.iter().any(|(k, v)| {
                    k.starts_with("_link_") && matches!(v, Value::Text(s) if s == &my_lid)
                })
        })
        .collect();

    if !children.is_empty() {
        let related: Vec<JsonValue> = children
            .iter()
            .map(|child| {
                let mut child_json = build_nested_node(child, all_records);
                // Add _relation field from the _link_ key name
                if let Some((link_key, _)) = child.fields.iter().find(|(k, v)| {
                    k.starts_with("_link_") && matches!(v, Value::Text(s) if s == &my_lid)
                }) {
                    let relation = link_key.trim_start_matches("_link_");
                    child_json["_relation"] = json!(relation);
                }
                child_json
            })
            .collect();
        obj["_related"] = json!(related);
    }

    obj
}

// ─── Value conversion ───────────────────────────────────────────────────────

pub fn record_to_json(record: &Record) -> JsonValue {
    let mut obj = serde_json::Map::new();
    obj.insert("lid".to_string(), json!(record.lid.to_string()));
    obj.insert("lobe".to_string(), json!(&record.lobe_name));

    for (k, v) in &record.fields {
        // Skip _link_ fields in JSON output (relationship is shown via _related)
        if k.starts_with("_link_") {
            continue;
        }
        obj.insert(k.clone(), value_to_json(v));
    }

    JsonValue::Object(obj)
}

fn value_to_json(value: &Value) -> JsonValue {
    match value {
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Text(s) => json!(s),
        Value::Timestamp(ts) => {
            // ISO 8601 approximate format
            json!(format!("ts:{}", ts))
        }
        Value::Bytes(b) => json!(format!("<{} bytes>", b.len())),
        Value::List(l) => {
            let items: Vec<JsonValue> = l.iter().map(value_to_json).collect();
            json!(items)
        }
        Value::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in m {
                obj.insert(k.clone(), value_to_json(v));
            }
            JsonValue::Object(obj)
        }
        Value::Null => JsonValue::Null,
        Value::Vector(packed) => {
            let items: Vec<JsonValue> = packed.iter().map(|x| json!(*x as f64)).collect();
            json!(items)
        }
    }
}

fn error_code(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("parse") {
        "PARSE_ERROR"
    } else if lower.contains("lobe") && lower.contains("not found") {
        "LOBE_NOT_FOUND"
    } else if lower.contains("record") && lower.contains("not found") {
        "RECORD_NOT_FOUND"
    } else if lower.contains("duplicate") {
        "DUPLICATE_ANCHOR"
    } else if lower.contains("type error") {
        "TYPE_ERROR"
    } else if lower.contains("invalid query") {
        "INVALID_QUERY"
    } else if lower.contains("storage") {
        "STORAGE_ERROR"
    } else if lower.contains("throttl") || lower.contains("pause") {
        "THROTTLED"
    } else {
        "INTERNAL_ERROR"
    }
}

#[cfg(test)]
mod budget_stop_wire {
    //! M2.3 flag wire contract: `budget_stop` is emitted ONLY when present (a
    //! NEAREST truncated by the latency airbag) and ABSENT otherwise, so ordinary
    //! pagination frames stay byte-identical — the additive guarantee that keeps
    //! existing clients (devva `clean_page` keys off `has_more`) working untouched.
    use super::*;
    use std::time::Duration;
    use xyzdb_core::result::BudgetStop;

    fn json_of(qr: &QueryResult) -> JsonValue {
        serde_json::from_slice(&serialize_json(qr, Duration::from_millis(1), false, None)).unwrap()
    }

    #[test]
    fn budget_stop_present_and_shaped_when_some() {
        let j = json_of(&QueryResult::PaginatedRecords {
            records: vec![],
            cursor: None,
            has_more: true,
            budget_stop: Some(BudgetStop {
                strategy: xyzdb_core::result::ScanStrategy::ScoreOrder,
                examined: 238_000,
                candidates: 246_000,
                found: 6,
            }),
        });
        assert_eq!(j["has_more"], json!(true));
        assert_eq!(j["budget_stop"]["examined"], json!(238_000));
        assert_eq!(j["budget_stop"]["candidates"], json!(246_000));
        assert_eq!(j["budget_stop"]["found"], json!(6));
        // The discriminant must reach the wire as the plain word a consumer reads
        // (not an enum name): it is what distinguishes a PREFIX of the answer from
        // the best of a key region, and without it the documented "extrapolate the
        // pass rate to the tail" reading is false for one of the two orders.
        assert_eq!(j["budget_stop"]["strategy"], json!("score_order"));
    }

    #[test]
    fn budget_stop_absent_when_none() {
        // Ordinary cursor page: has_more present, budget_stop key ABSENT.
        let j = json_of(&QueryResult::PaginatedRecords {
            records: vec![],
            cursor: Some("tok".into()),
            has_more: true,
            budget_stop: None,
        });
        assert_eq!(j["has_more"], json!(true));
        assert_eq!(j["cursor"], json!("tok"));
        assert!(
            j.get("budget_stop").is_none(),
            "budget_stop must be absent (not null) when None, for byte-identical frames"
        );
    }
}
