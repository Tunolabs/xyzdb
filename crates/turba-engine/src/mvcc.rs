//! MVCC stream: filters a sorted entry stream to show only the latest
//! visible version of each user key.
//!
//! Input: entries sorted by (user_key ASC, seqno DESC) — the merge iterator output.
//! Output: for each unique user_key, only the first entry with seqno <= visible_seqno.
//!         Tombstones are included (caller decides whether to skip them).

use crate::merge_op::MergeOperator;
use crate::types::{Entry, SeqNo, ValueType};
use std::iter::Peekable;
use std::sync::Arc;

pub struct MvccStream<I: Iterator<Item = Entry>> {
    inner: Peekable<I>,
    visible_seqno: SeqNo,
    /// Optional per-key merge operator. For an owned key, the visible versions
    /// are folded into one value instead of newest-visible-wins. Non-owned keys
    /// (and all keys when `None`) keep the standard MVCC behaviour.
    merge_operator: Option<Arc<dyn MergeOperator>>,
}

impl<I: Iterator<Item = Entry>> MvccStream<I> {
    pub fn new(inner: I, visible_seqno: SeqNo) -> Self {
        Self::new_with_merge(inner, visible_seqno, None)
    }

    /// As [`new`](Self::new), with an optional merge operator that folds an
    /// owned key's visible versions on read.
    pub fn new_with_merge(
        inner: I,
        visible_seqno: SeqNo,
        merge_operator: Option<Arc<dyn MergeOperator>>,
    ) -> Self {
        Self {
            inner: inner.peekable(),
            visible_seqno,
            merge_operator,
        }
    }
}

impl<I: Iterator<Item = Entry>> Iterator for MvccStream<I> {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        loop {
            let entry = self.inner.next()?;
            let current_key = entry.key.clone();

            // Merge-owned key: collect the run and fold the visible versions
            // newer than the most recent (visible) tombstone.
            if self
                .merge_operator
                .as_ref()
                .is_some_and(|op| op.owns(&current_key))
            {
                let op = self.merge_operator.as_ref().unwrap();
                let mut run = vec![entry];
                while let Some(peeked) = self.inner.peek() {
                    if peeked.key == current_key {
                        run.push(self.inner.next().unwrap());
                    } else {
                        break;
                    }
                }
                let mut live: Vec<&[u8]> = Vec::new();
                let mut newest_live_seqno = 0u64;
                let mut newest_visible_tombstone: Option<Entry> = None;
                // Fold each DISTINCT seqno once. The same (key, seqno) write can
                // appear in two sources transiently — e.g. a sealed memtable and
                // the SST it is being flushed into both live in the snapshot
                // until the version swap removes the memtable. Last-writer-wins
                // dedups such duplicates implicitly; a merge fold must too, or it
                // double-counts the delta. The stream is seqno-descending, so
                // duplicates are adjacent.
                let mut last_seqno: Option<SeqNo> = None;
                for e in &run {
                    if e.seqno > self.visible_seqno {
                        continue; // invisible at this snapshot
                    }
                    if last_seqno == Some(e.seqno) {
                        continue; // duplicate of an already-folded write
                    }
                    last_seqno = Some(e.seqno);
                    if e.value_type == ValueType::Tombstone {
                        if live.is_empty() {
                            newest_visible_tombstone = Some(e.clone());
                        }
                        break; // a delete resets the chain; older versions are gone
                    }
                    if live.is_empty() {
                        newest_live_seqno = e.seqno;
                    }
                    live.push(e.value.as_slice());
                }
                if !live.is_empty() {
                    let merged = op.merge(&current_key, &live);
                    return Some(Entry::new(
                        current_key,
                        merged,
                        newest_live_seqno,
                        ValueType::Value,
                    ));
                }
                if let Some(t) = newest_visible_tombstone {
                    return Some(t); // caller filters Value-only → key gone
                }
                continue; // nothing visible for this key
            }

            if entry.seqno <= self.visible_seqno {
                // This is the visible version. Skip all remaining versions of this key.
                while let Some(peeked) = self.inner.peek() {
                    if peeked.key == current_key {
                        self.inner.next();
                    } else {
                        break;
                    }
                }
                return Some(entry);
            }

            // seqno > visible_seqno — this version is invisible.
            // Skip remaining versions of this key until we find a visible one
            // or exhaust all versions.
            loop {
                match self.inner.peek() {
                    Some(peeked) if peeked.key == current_key => {
                        let next = self.inner.next().unwrap();
                        if next.seqno <= self.visible_seqno {
                            // Found visible version. Skip rest of this key.
                            while let Some(p) = self.inner.peek() {
                                if p.key == current_key {
                                    self.inner.next();
                                } else {
                                    break;
                                }
                            }
                            return Some(next);
                        }
                        // Still invisible, continue
                    }
                    _ => break, // no more versions of this key, move to next key
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ValueType;

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
            vec![values.iter().map(|v| v[0] as u64).sum::<u64>() as u8]
        }
    }

    fn op() -> Option<Arc<dyn MergeOperator>> {
        Some(Arc::new(SumOp))
    }

    #[test]
    fn merge_fold_owned_key() {
        let entries = vec![
            val_b(b"Rg", &[5], 3, ValueType::Value),
            val_b(b"Rg", &[2], 2, ValueType::Value),
            val_b(b"Rg", &[1], 1, ValueType::Value),
            val_b(b"b", b"x", 1, ValueType::Value),
        ];
        let r: Vec<_> = MvccStream::new_with_merge(entries.into_iter(), u64::MAX, op()).collect();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].value, vec![8]); // 5+2+1
        assert_eq!(r[1].value, b"x"); // plain key untouched
    }

    #[test]
    fn merge_fold_respects_visibility() {
        let entries = vec![
            val_b(b"Rg", &[5], 3, ValueType::Value),
            val_b(b"Rg", &[2], 2, ValueType::Value),
            val_b(b"Rg", &[1], 1, ValueType::Value),
        ];
        // Only seq <= 2 visible → the seq-3 delta is excluded from the fold.
        let r: Vec<_> = MvccStream::new_with_merge(entries.into_iter(), 2, op()).collect();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].value, vec![3]); // 2+1
    }

    #[test]
    fn merge_fold_tombstone_resets_and_hides() {
        // Live above the tombstone folds; deleted-older is excluded.
        let live = vec![
            val_b(b"Rg", &[7], 5, ValueType::Value),
            val_b(b"Rg", b"", 4, ValueType::Tombstone),
            val_b(b"Rg", &[3], 3, ValueType::Value),
        ];
        let r: Vec<_> = MvccStream::new_with_merge(live.into_iter(), u64::MAX, op())
            .filter(|e| e.value_type == ValueType::Value)
            .collect();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].value, vec![7]);

        // Newest visible is a tombstone → key is gone.
        let gone = vec![
            val_b(b"Rg", b"", 5, ValueType::Tombstone),
            val_b(b"Rg", &[3], 3, ValueType::Value),
        ];
        let r2: Vec<_> = MvccStream::new_with_merge(gone.into_iter(), u64::MAX, op())
            .filter(|e| e.value_type == ValueType::Value)
            .collect();
        assert!(r2.is_empty());
    }

    #[test]
    fn mvcc_latest_version_only() {
        let entries = vec![
            val("a", "a3", 3, ValueType::Value),
            val("a", "a2", 2, ValueType::Value),
            val("a", "a1", 1, ValueType::Value),
            val("b", "b2", 2, ValueType::Value),
            val("b", "b1", 1, ValueType::Value),
        ];

        let result: Vec<_> = MvccStream::new(entries.into_iter(), u64::MAX).collect();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].value, b"a3");
        assert_eq!(result[1].value, b"b2");
    }

    #[test]
    fn mvcc_visibility_cutoff() {
        let entries = vec![
            val("key", "v3", 30, ValueType::Value),
            val("key", "v2", 20, ValueType::Value),
            val("key", "v1", 10, ValueType::Value),
        ];

        // Visible at seqno 25 → sees v2
        let result: Vec<_> = MvccStream::new(entries.clone().into_iter(), 25).collect();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value, b"v2");

        // Visible at seqno 5 → sees nothing
        let result: Vec<_> = MvccStream::new(entries.into_iter(), 5).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn mvcc_tombstone_visible() {
        let entries = vec![
            val("key", "", 10, ValueType::Tombstone),
            val("key", "old", 5, ValueType::Value),
        ];

        let result: Vec<_> = MvccStream::new(entries.into_iter(), u64::MAX).collect();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].value_type, ValueType::Tombstone);
    }

    #[test]
    fn mvcc_interleaved_keys() {
        let entries = vec![
            val("a", "a2", 20, ValueType::Value),
            val("a", "a1", 10, ValueType::Value),
            val("b", "b1", 15, ValueType::Value),
            val("c", "c3", 30, ValueType::Value),
            val("c", "c1", 10, ValueType::Value),
        ];

        let result: Vec<_> = MvccStream::new(entries.into_iter(), u64::MAX).collect();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].key, b"a");
        assert_eq!(result[1].key, b"b");
        assert_eq!(result[2].key, b"c");
    }
}
