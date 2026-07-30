use crate::lid::LID;
use crate::record::Record;
use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Result of a query execution. Shared between engine, server, and clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueryResult {
    /// Single mutation succeeded (PUT/SET/DELETE/LINK/ANCHOR/LOBE).
    Ok { lid: Option<LID>, message: String },
    /// Batch mutation succeeded.
    BatchOk {
        count: usize,
        first_lid: LID,
        last_lid: LID,
    },
    /// FIND/PULL/SCAN: records found.
    Records(Vec<Record>),
    /// AGGREGATE: computed values.
    Aggregation(BTreeMap<String, Value>),
    /// SHOW: metadata info lines.
    Info(Vec<String>),
    /// V4: GROUP BY + AGGREGATE: each entry has group key fields + aggregate results.
    GroupedAggregation(Vec<BTreeMap<String, Value>>),
    /// v0.2.5.1: paginated SCAN result. Returned when the caller passed
    /// `CURSOR "<token>"` OR when the engine's default LIMIT capped a
    /// result set and more records remain. Plain `Records` continues to
    /// be returned when a SCAN fits completely under the active LIMIT —
    /// existing clients that never paginate see no shape change.
    ///
    /// Bincode wire form: variant index follows the existing six variants.
    /// Old clients decoding a `PaginatedRecords` frame fail explicitly;
    /// `Records` frames remain byte-identical.
    PaginatedRecords {
        records: Vec<Record>,
        /// Opaque token for the next page. `None` when `has_more` is false.
        cursor: Option<String>,
        /// `true` when the engine detected at least one record beyond the
        /// returned page.
        has_more: bool,
    },
}

impl QueryResult {
    /// Extract records from a Records (or PaginatedRecords) result, or
    /// empty vec for other variants. Pagination metadata is dropped.
    pub fn into_records(self) -> Vec<Record> {
        match self {
            QueryResult::Records(recs) => recs,
            QueryResult::PaginatedRecords { records, .. } => records,
            _ => vec![],
        }
    }
}
