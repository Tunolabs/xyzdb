//! Metric-ordered rollups for ghosts declared `ORDER BY <metric>`.
//!
//! A grouped-aggregate ghost normally stores one rollup per group keyed by the
//! GROUP BY key (`[ROLLUP][ghost_id][group_key]`). Serving `TOP n BY <metric>`
//! from that means reading every group (O(M)) and quickselecting. When the ghost
//! also declares `ORDER BY <metric>`, this module maintains a SECOND rollup keyed
//! by the metric value (`[METRIC_ORDER][ghost_id][enc(metric)][tiebreak]`), so a
//! `TOP n` reads only the first N entries — O(N).
//!
//! The order is a *frozen snapshot of the metric at REFRESH/CREATE time*: the
//! entry's value carries the group's finalized state, so a `TOP` served from it
//! is a consistent as-of-last-pass result and does NOT fall back to O(M) on later
//! writes. Freshness is bounded (see `order_emitted_at` in `GhostMeta`) and
//! surfaced by `SHOW GHOSTS`. The write path is never touched: entries are
//! blind-inserted and compaction sorts them.
//!
//! Bit-identity: [`group_state_to_row`], [`top_metric_f64`] and
//! [`top_tiebreak_key`] are the single source of truth for how a group becomes a
//! result row, its metric, and its tiebreak — shared by emission, the O(N) read,
//! and `planner::apply_top`. So the O(N) result equals sort-all-then-truncate.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use xyzdb_core::value::Value;

use super::*;
use crate::aggregate_state::{AggregateState, RollupDelta, decode_rollup_delta};

/// The `ORDER BY <metric>` declaration on a ghost: which aggregate metric its
/// groups are also kept ordered by, and the direction. `label` is the canonical
/// aggregate label (the same one a `TOP n BY` resolves to).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct MetricOrder {
    pub label: String,
    pub descending: bool,
}

// ─── Order-preserving metric encoding ────────────────────────────────────────

/// Order-preserving 8-byte encoding of an `f64` whose ascending byte order equals
/// `f64::total_cmp` ascending (the exact comparator `apply_top` uses). Standard
/// IEEE-754 total-order transform: flip all bits when the sign bit is set,
/// otherwise flip only the sign bit.
fn f64_total_order_bytes(x: f64) -> [u8; 8] {
    let bits = x.to_bits();
    let ord = if bits >> 63 == 1 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    };
    ord.to_be_bytes()
}

/// Encode the metric for a METRIC_ORDER key so an ascending key scan yields the
/// requested order: DESC (default) = largest metric first, ASC = smallest first.
pub(crate) fn enc_metric(metric: f64, descending: bool) -> [u8; 8] {
    let mut b = f64_total_order_bytes(metric);
    if descending {
        for byte in b.iter_mut() {
            *byte = !*byte;
        }
    }
    b
}

/// Full METRIC_ORDER key: `[METRIC_ORDER][ghost_id:2][enc(metric):8][tiebreak]`.
/// The tiebreak (the `apply_top` group-key string, always ascending) breaks equal
/// metrics exactly as `apply_top` does, so the byte order matches the comparator.
fn metric_order_key(ghost_id: u16, metric: f64, descending: bool, tiebreak: &str) -> Vec<u8> {
    let tb = tiebreak.as_bytes();
    let mut key = Vec::with_capacity(4 + 8 + tb.len());
    key.extend_from_slice(&crate::reserved_keys::METRIC_ORDER);
    key.extend_from_slice(&ghost_id.to_be_bytes());
    key.extend_from_slice(&enc_metric(metric, descending));
    key.extend_from_slice(tb);
    key
}

/// Prefix covering every metric-ordered entry of one ghost.
pub(crate) fn metric_order_ghost_prefix(ghost_id: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&crate::reserved_keys::METRIC_ORDER);
    p.extend_from_slice(&ghost_id.to_be_bytes());
    p
}

// ─── Entry value: (group_key, frozen state) ──────────────────────────────────

/// Encode a METRIC_ORDER entry value: `[gk_len:u16][gk][RollupDelta::encode()]`.
/// Carries the group key (to rebuild the row's group fields) plus the frozen
/// aggregate state (as a positive delta), so the read reproduces the exact row.
fn encode_value(gk: &str, delta: &RollupDelta) -> Vec<u8> {
    let gkb = gk.as_bytes();
    let gklen = gkb.len().min(u16::MAX as usize);
    let db = delta.encode();
    let mut v = Vec::with_capacity(2 + gklen + db.len());
    v.extend_from_slice(&(gklen as u16).to_be_bytes());
    v.extend_from_slice(&gkb[..gklen]);
    v.extend_from_slice(&db);
    v
}

/// Inverse of [`encode_value`]: `(group_key, folded state)`, or `None` if malformed.
fn decode_value(bytes: &[u8]) -> Option<(String, AggregateState)> {
    if bytes.len() < 2 {
        return None;
    }
    let gklen = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let gk = std::str::from_utf8(bytes.get(2..2 + gklen)?)
        .ok()?
        .to_string();
    let delta = decode_rollup_delta(bytes.get(2 + gklen..)?)?;
    Some((gk, delta.into_aggregate_state()))
}

// ─── Shared row / metric / tiebreak (bit-identity linchpin) ──────────────────

/// Numeric coercion for a metric value — the same rule `apply_top` uses.
pub(crate) fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Build a grouped-result row from a group key + its aggregate state. The single
/// definition shared by the O(M) path (`scan::execute_scan_group_aggregate`),
/// emission, and the O(N) read — so all three produce byte-identical rows.
pub(crate) fn group_state_to_row(
    group_fields: &[String],
    gk: &str,
    state: &AggregateState,
) -> BTreeMap<String, Value> {
    let mut row = BTreeMap::new();
    let parts = crate::aggregate_state::decode_group_key(gk);
    for (i, field) in group_fields.iter().enumerate() {
        if let Some(frag) = parts.get(i) {
            row.insert(
                field.clone(),
                crate::aggregate_state::group_key_fragment_to_value(frag),
            );
        }
    }
    for (k, v) in state.to_result() {
        row.insert(k, v);
    }
    row
}

/// The metric a row is ranked by (missing / non-numeric sorts last under DESC).
pub(crate) fn top_metric_f64(row: &BTreeMap<String, Value>, label: &str) -> f64 {
    row.get(label)
        .and_then(value_to_f64)
        .unwrap_or(f64::NEG_INFINITY)
}

/// The `apply_top` secondary tiebreak key: group fields formatted and `|`-joined,
/// always ascending. Groups are assumed distinct here (one row per group).
pub(crate) fn top_tiebreak_key(group_fields: &[String], row: &BTreeMap<String, Value>) -> String {
    group_fields
        .iter()
        .map(|f| row.get(f).map(|v| format!("{v}")).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("|")
}

// ─── Emission (full re-emit on CREATE / REFRESH) ─────────────────────────────

/// (Re)build the metric-ordered rollup for a ghost from its finalized group
/// state. Full re-emit — REFRESH drops+recreates under a fresh `ghost_id`, so the
/// old range is already gone; a plain CREATE starts empty. Blind-inserts; the
/// dictionary's compaction sorts the entries into metric order.
///
/// `in_ram` is `Some(map)` for an in-RAM ghost (iterate it) and `None` for a
/// spilled ghost (read the finalized rollups back from disk). Returns `Ok(true)`
/// on a clean emit, `Ok(false)` if two groups collided on (metric, tiebreak) —
/// the order would be ambiguous (matching `apply_top`'s own non-determinism) and
/// a blind insert would LWW-drop one group, so the caller marks the order stale
/// and reads fall back to O(M).
pub(crate) fn emit_metric_order(
    dictionary: &Tree,
    ghost_id: u16,
    group_fields: &[String],
    order: &MetricOrder,
    in_ram: Option<&BTreeMap<String, AggregateState>>,
) -> Result<bool> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut ok = true;

    let mut emit_one = |gk: &str, state: &AggregateState| -> Result<()> {
        let row = group_state_to_row(group_fields, gk, state);
        let metric = top_metric_f64(&row, &order.label);
        let tiebreak = top_tiebreak_key(group_fields, &row);
        let key = metric_order_key(ghost_id, metric, order.descending, &tiebreak);
        // Collision on the ordering part (metric + tiebreak): the slot is
        // ambiguous. Flag it; a blind insert would silently drop a group.
        if !seen.insert(key[4..].to_vec()) {
            ok = false;
        }
        let value = encode_value(gk, &RollupDelta::from_aggregate_state(state));
        dictionary
            .insert(&key, &value)
            .map_err(|e| XyzError::Storage(format!("metric-order emit: {e}")))?;
        Ok(())
    };

    match in_ram {
        Some(map) => {
            for (gk, state) in map {
                if state.count > 0 {
                    emit_one(gk, state)?;
                }
            }
        }
        None => {
            for entry in dictionary
                .prefix_iter(&rollup_ghost_prefix(ghost_id))
                .map_err(|e| XyzError::Storage(format!("metric-order source scan: {e}")))?
            {
                let Some(gk) = rollup_key_group(&entry.key) else {
                    continue;
                };
                if let Some(d) = decode_rollup_delta(&entry.value) {
                    let state = d.into_aggregate_state();
                    if state.count > 0 {
                        emit_one(gk, &state)?;
                    }
                }
            }
        }
    }
    Ok(ok)
}

// ─── O(N) read ───────────────────────────────────────────────────────────────

/// Read the top-`n` groups straight from the metric-ordered rollup, in order.
/// Rows are byte-identical to the O(M) path + `apply_top` (same builder). Caller
/// must have already checked the ghost's declared order matches the query's
/// metric + direction and that the order is emitted (not stale).
pub(crate) fn read_topn(
    dictionary: &Tree,
    ghost_id: u16,
    group_fields: &[String],
    n: usize,
) -> Result<Vec<BTreeMap<String, Value>>> {
    let mut rows = Vec::with_capacity(n);
    for entry in dictionary
        .prefix_iter(&metric_order_ghost_prefix(ghost_id))
        .map_err(|e| XyzError::Storage(format!("metric-order read: {e}")))?
        .take(n)
    {
        if let Some((gk, state)) = decode_value(&entry.value) {
            rows.push(group_state_to_row(group_fields, &gk, &state));
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The metric encoding must sort byte-lexicographically exactly as
    /// `f64::total_cmp` (ascending for ASC, reversed for DESC) — including
    /// negatives, ±0.0 and ties. This is what makes the O(N) read bit-identical.
    #[test]
    fn enc_metric_orders_like_total_cmp() {
        let vals = [
            f64::MIN,
            -1e18,
            -1000.5,
            -1.0,
            -0.0,
            0.0,
            1.0,
            2.0,
            2.0,
            41.999,
            42.0,
            1000.5,
            1e18,
            f64::MAX,
        ];
        for &descending in &[false, true] {
            let mut by_bytes: Vec<f64> = vals.to_vec();
            by_bytes.sort_by(|a, b| enc_metric(*a, descending).cmp(&enc_metric(*b, descending)));
            let mut expected: Vec<f64> = vals.to_vec();
            expected.sort_by(|a, b| {
                if descending {
                    b.total_cmp(a)
                } else {
                    a.total_cmp(b)
                }
            });
            for (g, e) in by_bytes.iter().zip(expected.iter()) {
                assert_eq!(
                    g.total_cmp(e),
                    std::cmp::Ordering::Equal,
                    "descending={descending}: byte order diverges from total_cmp ({g} vs {e})"
                );
            }
        }
    }

    /// The full key (metric + tiebreak) must sort exactly as `apply_top`'s
    /// comparator: metric by direction, then group-key string ascending — even
    /// across metric ties.
    #[test]
    fn key_order_matches_apply_top_comparator() {
        let rows = [
            (100.0_f64, "g00"),
            (50.0, "g03"),
            (50.0, "g01"),
            (50.0, "g02"),
            (10.0, "g04"),
            (-5.0, "g05"),
        ];
        for &descending in &[false, true] {
            let mut by_key: Vec<(f64, &str)> = rows.to_vec();
            by_key.sort_by(|a, b| {
                metric_order_key(1, a.0, descending, a.1)
                    .cmp(&metric_order_key(1, b.0, descending, b.1))
            });
            let mut expected: Vec<(f64, &str)> = rows.to_vec();
            expected.sort_by(|a, b| {
                let primary = if descending {
                    b.0.total_cmp(&a.0)
                } else {
                    a.0.total_cmp(&b.0)
                };
                primary.then_with(|| a.1.cmp(b.1))
            });
            assert_eq!(by_key, expected, "descending={descending}");
        }
    }

    /// The entry value round-trips the group key + folded state.
    #[test]
    fn value_round_trips() {
        let st = RollupDelta {
            count: 3,
            ..Default::default()
        }
        .into_aggregate_state();
        let bytes = encode_value("3:sabc", &RollupDelta::from_aggregate_state(&st));
        let (gk, back) = decode_value(&bytes).expect("decode");
        assert_eq!(gk, "3:sabc");
        assert_eq!(back.count, 3);
    }
}
