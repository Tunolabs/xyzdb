// SPDX-License-Identifier: BUSL-1.1
use crate::engine::{Engine, QueryResult};
use std::collections::BTreeMap;
use xytalk_parser::ast::{FetchStmt, FindTarget};
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::lid::LID;
use xyzdb_core::record::Record;
use xyzdb_core::value::Value;

/// Execute FETCH: resolve the shared `WHERE` against each named lobe and return
/// one record whose fields are a named section per lobe (a `List` of the
/// matching records, each as a `Map` of its fields).
///
/// This is packaging, not composition: every section is exactly what the same
/// `FIND`/`SCAN WHERE` on that lobe would return, gathered into one result so an
/// N-entity context (a customer plus their credits plus their operations) costs
/// one call instead of N. Because the lobes co-locate by the shared key, each
/// resolve rides the anchor/gravity fast path.
///
/// # Errors
/// Returns `Error::InvalidQuery` if `WHERE` is absent or an `AS` name list does
/// not have one entry per lobe, and propagates `LobeNotFound` for a missing lobe.
pub fn execute_fetch(engine: &Engine, stmt: FetchStmt) -> Result<QueryResult> {
    if stmt.filter_expr.is_none() {
        return Err(XyzError::InvalidQuery(
            "FETCH requires WHERE — add the shared predicate that selects the \
             co-located records across the lobes"
                .into(),
        ));
    }
    let names: Vec<String> = match &stmt.names {
        Some(ns) => {
            if ns.len() != stmt.lobes.len() {
                return Err(XyzError::InvalidQuery(format!(
                    "FETCH AS lists {} name(s) for {} lobe(s) — one section name per lobe",
                    ns.len(),
                    stmt.lobes.len()
                )));
            }
            ns.clone()
        }
        None => stmt.lobes.clone(),
    };

    let mut sections: BTreeMap<String, Value> = BTreeMap::new();
    for (lobe, name) in stmt.lobes.iter().zip(names) {
        let target = FindTarget::Lobe(lobe.clone());
        let found = engine.resolve_find_expr(&target, &stmt.filter_expr)?;
        let rows: Vec<Value> = found
            .into_iter()
            .map(|(r, _)| Value::Map(r.fields))
            .collect();
        sections.insert(name, Value::List(rows));
    }

    // One synthetic envelope record carrying the named sections. It is a
    // transport shape, not a stored record: nil LID, no timestamps.
    let envelope = Record {
        lid: LID::from_raw(0),
        lobe_name: "fetch".into(),
        fields: sections,
        created_at: 0,
        updated_at: 0,
    };
    Ok(QueryResult::Records(vec![envelope]))
}
