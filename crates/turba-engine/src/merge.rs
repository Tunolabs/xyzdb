//! K-way merge iterator: merges multiple sorted iterators into a single sorted stream.
//!
//! Uses a binary min-heap. Each source provides entries in InternalKey order
//! (user_key ASC, seqno DESC). The merge iterator produces the globally
//! sorted stream by always popping the minimum.

// SPDX-License-Identifier: BUSL-1.1
use crate::types::Entry;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

struct HeapItem {
    entry: Entry,
    source_idx: usize,
}

// BinaryHeap is a max-heap, so we reverse the ordering for min-heap behavior.
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: we want the smallest InternalKey at the top
        let key_cmp = self
            .entry
            .key
            .cmp(&other.entry.key)
            .then_with(|| other.entry.seqno.cmp(&self.entry.seqno)); // seqno DESC

        // Reverse for min-heap
        key_cmp.reverse()
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.entry.key == other.entry.key && self.entry.seqno == other.entry.seqno
    }
}

impl Eq for HeapItem {}

pub struct MergeIterator {
    heap: BinaryHeap<HeapItem>,
    sources: Vec<Box<dyn Iterator<Item = Entry>>>,
}

impl MergeIterator {
    /// Create a merge iterator from multiple sorted sources.
    pub fn new(sources: Vec<Box<dyn Iterator<Item = Entry>>>) -> Self {
        let mut heap = BinaryHeap::with_capacity(sources.len());
        let mut stored_sources: Vec<Box<dyn Iterator<Item = Entry>>> =
            Vec::with_capacity(sources.len());

        for (idx, mut source) in sources.into_iter().enumerate() {
            if let Some(entry) = source.next() {
                heap.push(HeapItem {
                    entry,
                    source_idx: idx,
                });
            }
            stored_sources.push(source);
        }

        Self {
            heap,
            sources: stored_sources,
        }
    }
}

impl Iterator for MergeIterator {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        let item = self.heap.pop()?;
        let source_idx = item.source_idx;

        // Pull next from the same source
        if let Some(next_entry) = self.sources[source_idx].next() {
            self.heap.push(HeapItem {
                entry: next_entry,
                source_idx,
            });
        }

        Some(item.entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ValueType;

    fn entry(key: &str, seqno: u64) -> Entry {
        Entry::new(
            key.as_bytes().to_vec(),
            format!("v{seqno}").into_bytes(),
            seqno,
            ValueType::Value,
        )
    }

    #[test]
    fn merge_two_sources() {
        let s1: Vec<Entry> = vec![entry("a", 1), entry("c", 1), entry("e", 1)];
        let s2: Vec<Entry> = vec![entry("b", 1), entry("d", 1), entry("f", 1)];

        let sources: Vec<Box<dyn Iterator<Item = Entry>>> =
            vec![Box::new(s1.into_iter()), Box::new(s2.into_iter())];

        let merged: Vec<_> = MergeIterator::new(sources).collect();
        let keys: Vec<_> = merged.iter().map(|e| e.key.clone()).collect();
        assert_eq!(
            keys,
            vec![b"a", b"b", b"c", b"d", b"e", b"f"]
                .into_iter()
                .map(|b| b.to_vec())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn merge_same_key_seqno_desc() {
        // Two sources with same key, different seqnos
        let s1 = vec![entry("key", 10)];
        let s2 = vec![entry("key", 5)];

        let sources: Vec<Box<dyn Iterator<Item = Entry>>> =
            vec![Box::new(s1.into_iter()), Box::new(s2.into_iter())];

        let merged: Vec<_> = MergeIterator::new(sources).collect();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].seqno, 10); // higher seqno first
        assert_eq!(merged[1].seqno, 5);
    }

    #[test]
    fn merge_empty_sources() {
        let sources: Vec<Box<dyn Iterator<Item = Entry>>> =
            vec![Box::new(std::iter::empty()), Box::new(std::iter::empty())];
        let merged: Vec<_> = MergeIterator::new(sources).collect();
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_single_source() {
        let s1 = vec![entry("a", 1), entry("b", 2), entry("c", 3)];
        let sources: Vec<Box<dyn Iterator<Item = Entry>>> = vec![Box::new(s1.into_iter())];
        let merged: Vec<_> = MergeIterator::new(sources).collect();
        assert_eq!(merged.len(), 3);
    }
}
