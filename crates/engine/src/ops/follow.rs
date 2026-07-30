//! `FOLLOW` pipeline step — cross-entity (cross-bucket) expansion.
//!
//! For each current record, take its `field` value and resolve it as
//! `target_field` in `lobe` (via the FIND fast path — anchor/gravity lookup),
//! fetching those records. This is the relational bridge ACROSS gravity buckets
//! that `PULL` cannot make (PULL stays inside one bucket): "chat message → its
//! cited document (a different entity)" becomes one pipeline step. The caller
//! names the reference field and target — the engine just resolves it cheaply.

use crate::engine::Engine;
use std::collections::HashSet;
use xytalk_parser::ast::{Filter, FilterOp, FindTarget, FollowStmt, Literal};
use xyzdb_core::error::Result;
use xyzdb_core::record::Record;
use xyzdb_core::value::Value;

/// Resolve each record's `field` reference into `lobe`/`target_field` and return
/// the fetched records, deduplicated by reference value and by LID. A record
/// whose `field` is missing or not text is skipped.
///
/// # Errors
/// Propagates resolution errors from the target FIND.
pub fn execute_follow(
    engine: &Engine,
    records: Vec<Record>,
    stmt: &FollowStmt,
) -> Result<Vec<Record>> {
    let target = FindTarget::Lobe(stmt.lobe.clone());
    let mut seen_vals: HashSet<String> = HashSet::new();
    let mut seen_lids: HashSet<[u8; 16]> = HashSet::new();
    let mut out = Vec::new();
    for rec in &records {
        let val = match rec.fields.get(&stmt.field) {
            Some(Value::Text(t)) => t.clone(),
            _ => continue, // no reference / not a text reference
        };
        if !seen_vals.insert(val.clone()) {
            continue; // this reference already followed
        }
        let filters = [Filter {
            field: stmt.target_field.clone(),
            op: FilterOp::Eq,
            value: Literal::Text(val),
        }];
        for (r, _) in engine.resolve_find(&target, &filters)? {
            if seen_lids.insert(r.lid.to_bytes()) {
                out.push(r);
            }
        }
    }
    Ok(out)
}
