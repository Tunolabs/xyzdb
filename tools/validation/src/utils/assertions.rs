use anyhow::{Result, bail};
use xyzdb_core::result::QueryResult;

/// Assert that a QueryResult is Ok.
pub fn assert_ok(result: &QueryResult, context: &str) -> Result<()> {
    match result {
        QueryResult::Ok { .. } | QueryResult::BatchOk { .. } => Ok(()),
        QueryResult::Records(_) => Ok(()),
        QueryResult::Info(_) => Ok(()),
        QueryResult::Aggregation(_) => Ok(()),
    }
}

/// Extract record count from a Records result.
pub fn record_count(result: &QueryResult) -> usize {
    match result {
        QueryResult::Records(recs) => recs.len(),
        _ => 0,
    }
}

/// Extract count from text response by counting "LID:" occurrences.
pub fn count_lids_in_text(text: &str) -> usize {
    text.matches("LID:").count()
}

/// Extract aggregate value from Aggregation result.
pub fn get_aggregate_int(result: &QueryResult, key: &str) -> Option<i64> {
    match result {
        QueryResult::Aggregation(map) => {
            map.get(key).and_then(|v| v.as_int())
        }
        _ => None,
    }
}

pub fn get_aggregate_float(result: &QueryResult, key: &str) -> Option<f64> {
    match result {
        QueryResult::Aggregation(map) => {
            map.get(key).and_then(|v| match v {
                xyzdb_core::value::Value::Float(f) => Some(*f),
                xyzdb_core::value::Value::Int(i) => Some(*i as f64),
                _ => None,
            })
        }
        _ => None,
    }
}
