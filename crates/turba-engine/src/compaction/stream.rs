//! Compaction stream: filters entries during compaction.
//!
//! - Drops old versions of same key (keeps only highest seqno per user_key).
//! - Drops tombstones at the last level (no older data to shadow).
//! - Preserves tombstones at non-last levels (needed to shadow data in lower levels).

// SPDX-License-Identifier: BUSL-1.1
use crate::merge_op::MergeOperator;
use crate::types::{Entry, SeqNo, ValueType};
use std::collections::VecDeque;
use std::iter::Peekable;
use std::sync::Arc;

pub struct CompactionStream<I: Iterator<Item = Entry>> {
    inner: Peekable<I>,
    is_last_level: bool,
    #[allow(dead_code)] // reserved for future snapshot-aware GC
    gc_watermark: SeqNo,
    /// Optional per-key merge operator. When set, versions of an owned key are
    /// folded into one (plus a preserved tombstone at non-last levels) instead
    /// of last-writer-wins. Non-owned keys are unaffected.
    merge_operator: Option<Arc<dyn MergeOperator>>,
    /// Extra entries produced for one key (e.g. a merged value + its preserved
    /// tombstone) that haven't been yielded yet. Drained before advancing.
    pending: VecDeque<Entry>,
}

impl<I: Iterator<Item = Entry>> CompactionStream<I> {
    /// Create a compaction stream.
    /// - `is_last_level`: if true, tombstones are dropped (no data below to shadow).
    /// - `gc_watermark`: versions with seqno < watermark and not the latest are dropped.
    pub fn new(inner: I, is_last_level: bool, gc_watermark: SeqNo) -> Self {
        Self::new_with_merge(inner, is_last_level, gc_watermark, None)
    }

    /// As [`new`](Self::new), with an optional merge operator that folds an
    /// owned key's versions during compaction.
    pub fn new_with_merge(
        inner: I,
        is_last_level: bool,
        gc_watermark: SeqNo,
        merge_operator: Option<Arc<dyn MergeOperator>>,
    ) -> Self {
        Self {
            inner: inner.peekable(),
            is_last_level,
            gc_watermark,
            merge_operator,
            pending: VecDeque::new(),
        }
    }

    /// Collect the run of consecutive entries sharing `key` (the merge iterator
    /// yields them newest-first within a key). Caller already took the head.
    fn drain_same_key(&mut self, key: &[u8], head: Entry) -> Vec<Entry> {
        let mut run = vec![head];
        while let Some(peeked) = self.inner.peek() {
            if peeked.key.as_slice() == key {
                run.push(self.inner.next().unwrap());
            } else {
                break;
            }
        }
        run
    }
}

impl<I: Iterator<Item = Entry>> Iterator for CompactionStream<I> {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        if let Some(e) = self.pending.pop_front() {
            return Some(e);
        }
        loop {
            let entry = self.inner.next()?;
            let key = entry.key.clone();
            let owns = self.merge_operator.as_ref().is_some_and(|op| op.owns(&key));

            if owns {
                let run = self.drain_same_key(&key, entry);
                let op = self.merge_operator.as_ref().unwrap();
                // Live operands = values newer than the first tombstone (a
                // delete resets the merge chain; older deltas are gone).
                let tomb = run
                    .iter()
                    .position(|e| e.value_type == ValueType::Tombstone);
                let live_end = tomb.unwrap_or(run.len());
                let mut out: VecDeque<Entry> = VecDeque::new();
                if live_end > 0 {
                    // Fold each DISTINCT seqno once: the same (key, seqno) write
                    // can appear in two inputs transiently; folding both would
                    // double-count. The run is seqno-descending, so duplicates
                    // are adjacent.
                    let mut vals: Vec<&[u8]> = Vec::with_capacity(live_end);
                    let mut last_seqno: Option<SeqNo> = None;
                    for e in &run[..live_end] {
                        if last_seqno == Some(e.seqno) {
                            continue;
                        }
                        last_seqno = Some(e.seqno);
                        vals.push(e.value.as_slice());
                    }
                    let merged = op.merge(&key, &vals);
                    // Newest seqno carries the folded value.
                    out.push_back(Entry::new(
                        key.clone(),
                        merged,
                        run[0].seqno,
                        ValueType::Value,
                    ));
                }
                // Preserve the delete boundary so lower levels stay shadowed;
                // at the last level there is nothing below, so drop it.
                if let Some(ti) = tomb {
                    if !self.is_last_level {
                        out.push_back(run[ti].clone());
                    }
                }
                match out.pop_front() {
                    Some(first) => {
                        self.pending = out;
                        return Some(first);
                    }
                    None => continue, // last-level tombstone, no live deltas → drop
                }
            }

            // Not a merge key: last-writer-wins. Drop older versions of the key.
            while let Some(peeked) = self.inner.peek() {
                if peeked.key == key {
                    self.inner.next();
                } else {
                    break;
                }
            }
            // Drop tombstones at last level — no data below to shadow
            if self.is_last_level && entry.value_type == ValueType::Tombstone {
                continue;
            }
            return Some(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(key: &str, value: &str, seqno: u64, vtype: ValueType) -> Entry {
        Entry::new(
            key.as_bytes().to_vec(),
            value.as_bytes().to_vec(),
            seqno,
            vtype,
        )
    }

    fn val_b(key: &[u8], value: &[u8], seqno: u64, vtype: ValueType) -> Entry {
        Entry::new(key.to_vec(), value.to_vec(), seqno, vtype)
    }

    /// Toy operator: owns keys starting with `R`; combine = sum of first bytes.
    struct SumOp;
    impl MergeOperator for SumOp {
        fn owns(&self, key: &[u8]) -> bool {
            key.first() == Some(&b'R')
        }
        fn merge(&self, _key: &[u8], values: &[&[u8]]) -> Vec<u8> {
            let sum: u64 = values.iter().map(|v| v[0] as u64).sum();
            vec![sum as u8]
        }
    }

    #[test]
    fn merge_operator_folds_owned_key_run() {
        // 3 deltas for owned key "Rg" (newest-first), one plain key "b".
        let entries = vec![
            val_b(b"Rg", &[5], 3, ValueType::Value),
            val_b(b"Rg", &[2], 2, ValueType::Value),
            val_b(b"Rg", &[1], 1, ValueType::Value),
            val_b(b"b", b"x", 1, ValueType::Value),
        ];
        let op: Arc<dyn MergeOperator> = Arc::new(SumOp);
        let result: Vec<_> =
            CompactionStream::new_with_merge(entries.into_iter(), false, 0, Some(op)).collect();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].key, b"Rg");
        assert_eq!(result[0].value, vec![8]); // 5+2+1 folded
        assert_eq!(result[0].seqno, 3); // newest seqno carries the fold
        assert_eq!(result[1].key, b"b"); // plain key untouched
        assert_eq!(result[1].value, b"x");
    }

    #[test]
    fn merge_operator_leaves_non_owned_last_writer_wins() {
        let entries = vec![
            val_b(b"x", b"new", 2, ValueType::Value),
            val_b(b"x", b"old", 1, ValueType::Value),
        ];
        let op: Arc<dyn MergeOperator> = Arc::new(SumOp);
        let result: Vec<_> =
            CompactionStream::new_with_merge(entries.into_iter(), false, 0, Some(op)).collect();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value, b"new");
    }

    #[test]
    fn merge_operator_tombstone_resets_chain() {
        // newest value, a tombstone, then a deleted-older value.
        let mk = || {
            vec![
                val_b(b"Rg", &[7], 5, ValueType::Value),
                val_b(b"Rg", b"", 4, ValueType::Tombstone),
                val_b(b"Rg", &[3], 3, ValueType::Value),
            ]
        };
        // Non-last level: merged live ([7]) + preserved tombstone.
        let op: Arc<dyn MergeOperator> = Arc::new(SumOp);
        let r: Vec<_> =
            CompactionStream::new_with_merge(mk().into_iter(), false, 0, Some(op)).collect();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].value, vec![7]);
        assert_eq!(r[0].value_type, ValueType::Value);
        assert_eq!(r[1].value_type, ValueType::Tombstone);
        // Last level: tombstone dropped, only the merged live value remains.
        let op2: Arc<dyn MergeOperator> = Arc::new(SumOp);
        let r2: Vec<_> =
            CompactionStream::new_with_merge(mk().into_iter(), true, 0, Some(op2)).collect();
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].value, vec![7]);
    }

    #[test]
    fn keeps_latest_version_only() {
        let entries = vec![
            val("a", "a3", 3, ValueType::Value),
            val("a", "a2", 2, ValueType::Value),
            val("a", "a1", 1, ValueType::Value),
            val("b", "b1", 1, ValueType::Value),
        ];

        let result: Vec<_> = CompactionStream::new(entries.into_iter(), false, 0).collect();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].value, b"a3");
        assert_eq!(result[1].value, b"b1");
    }

    #[test]
    fn preserves_tombstone_non_last_level() {
        let entries = vec![
            val("key", "", 10, ValueType::Tombstone),
            val("key", "old", 5, ValueType::Value),
        ];

        let result: Vec<_> = CompactionStream::new(entries.into_iter(), false, 0).collect();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value_type, ValueType::Tombstone);
    }

    #[test]
    fn drops_tombstone_last_level() {
        let entries = vec![
            val("key", "", 10, ValueType::Tombstone),
            val("key", "old", 5, ValueType::Value),
        ];

        let result: Vec<_> = CompactionStream::new(entries.into_iter(), true, 0).collect();
        assert!(
            result.is_empty(),
            "tombstone at last level should be dropped"
        );
    }
}
