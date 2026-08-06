//! The ghost-rollup merge operator (hilo B).
//!
//! Set on the dictionary tree, it folds the blind-appended [`RollupDelta`]
//! values for a group's `[ROLLUP]…` key — at compaction (collapse the chain
//! into one) and at read (fold whatever chain remains). Keys outside the
//! `[ROLLUP]` prefix (anchors, gravity specs, pins) return `None`, so they keep
//! last-writer-wins. This is what lets rollups be written without a
//! read-modify-write: the O(groups) RMW that forced P0-2's revert.

// SPDX-License-Identifier: BUSL-1.1
use crate::aggregate_state::{RollupDelta, decode_rollup_delta};
use turba_engine::merge_op::MergeOperator;

/// Reserved prefix for ghost rollup keys (`[0xFF,0xF9]`).
const ROLLUP_PREFIX: [u8; 2] = crate::reserved_keys::ROLLUP;

/// Folds `RollupDelta` values for ghost-rollup keys; passes through everything
/// else to last-writer-wins.
pub struct RollupMergeOperator;

impl MergeOperator for RollupMergeOperator {
    fn owns(&self, key: &[u8]) -> bool {
        key.starts_with(&ROLLUP_PREFIX)
    }

    fn merge(&self, _key: &[u8], values_newest_first: &[&[u8]]) -> Vec<u8> {
        let mut folded = RollupDelta::default();
        // Associative + commutative, so newest-first order is irrelevant.
        for bytes in values_newest_first {
            if let Some(d) = decode_rollup_delta(bytes) {
                folded.merge(&d);
            }
        }
        folded.encode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate_state::AggregateState;
    use std::collections::BTreeMap;
    use xyzdb_core::value::Value;

    fn specs() -> Vec<crate::aggregate_state::Metric> {
        use crate::aggregate_state::{AggOp, COUNT_LABEL, Metric};
        vec![
            Metric::new(String::new(), AggOp::Count, COUNT_LABEL.to_string(), None),
            Metric::new("monto".into(), AggOp::Sum, "monto:Sum".to_string(), None),
        ]
    }

    fn rec(v: f64) -> xyzdb_core::record::Record {
        let mut m = BTreeMap::new();
        m.insert("monto".to_string(), Value::Float(v));
        xyzdb_core::record::Record {
            lid: xyzdb_core::lid::LID::new(1),
            lobe_name: String::new(),
            fields: m,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn owns_only_rollup_prefix() {
        let op = RollupMergeOperator;
        assert!(op.owns(&[0xFF, 0xF9, 1, 2, 3]));
        assert!(!op.owns(&[0xFF, 0xFA, 1])); // gravity spec prefix
        assert!(!op.owns(b"some-anchor-key"));
    }

    #[test]
    fn merge_folds_delta_chain() {
        let op = RollupMergeOperator;
        let s = specs();
        // Three add-deltas (as the LSM would hold them, newest-first).
        let vals: Vec<Vec<u8>> = [30.0, 20.0, 10.0]
            .iter()
            .map(|v| RollupDelta::for_record(&rec(*v), &s, 1).encode())
            .collect();
        let refs: Vec<&[u8]> = vals.iter().map(|v| v.as_slice()).collect();
        let folded = op.merge(&[0xFF, 0xF9, 0, 1], &refs);

        let st: AggregateState = decode_rollup_delta(&folded).unwrap().into_aggregate_state();
        assert_eq!(st.count, 3);
        match st.values.get("monto:Sum") {
            Some(crate::aggregate_state::AggValue::Sum(s)) => {
                assert!((s.to_f64() - 60.0).abs() < 1e-9)
            }
            _ => panic!("no Sum"),
        }
    }
}
