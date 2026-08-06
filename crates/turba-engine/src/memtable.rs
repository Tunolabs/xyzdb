//! Memtable: in-memory write buffer backed by a lock-free skip list.
//!
//! Entries are sorted by InternalKey (user_key ASC, seqno DESC).
//! When the approximate size exceeds a threshold, the memtable is sealed
//! and flushed to an SSTable on disk.

// SPDX-License-Identifier: BUSL-1.1
use crate::types::{Entry, InternalKey, SeqNo, ValueType};
use crossbeam_skiplist::SkipMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct Memtable {
    map: SkipMap<InternalKey, Vec<u8>>,
    approximate_size: AtomicUsize,
    highest_seqno: AtomicU64,
}

impl Memtable {
    pub fn new() -> Self {
        Self {
            map: SkipMap::new(),
            approximate_size: AtomicUsize::new(0),
            highest_seqno: AtomicU64::new(0),
        }
    }

    /// Insert a key-value pair. Returns the new approximate size.
    pub fn insert(&self, user_key: &[u8], value: &[u8], seqno: SeqNo, vtype: ValueType) -> usize {
        let ikey = InternalKey::new(user_key.to_vec(), seqno, vtype);
        let entry_size = user_key.len() + value.len() + 32; // key + value + overhead

        self.map.insert(ikey, value.to_vec());

        let new_size = self
            .approximate_size
            .fetch_add(entry_size, Ordering::AcqRel)
            + entry_size;
        self.highest_seqno.fetch_max(seqno, Ordering::AcqRel);
        new_size
    }

    /// Point lookup: returns the value for the highest seqno <= visible_seqno.
    pub fn get(&self, user_key: &[u8], visible_seqno: SeqNo) -> Option<(ValueType, Vec<u8>)> {
        // Seek to (user_key, SeqNo::MAX) — the highest possible seqno for this key.
        // Due to InternalKey ordering (seqno DESC), this finds the first entry for user_key.
        let lower = InternalKey::new(user_key.to_vec(), SeqNo::MAX, ValueType::Value);

        for entry in self.map.range(lower..) {
            let ikey = entry.key();
            if ikey.user_key != user_key {
                break; // past this user_key
            }
            if ikey.seqno <= visible_seqno {
                return Some((ikey.value_type, entry.value().clone()));
            }
        }
        None
    }

    /// Iterate all entries in sorted order (InternalKey ordering).
    pub fn iter(&self) -> impl Iterator<Item = Entry> + '_ {
        self.map.iter().map(|e| {
            let ikey = e.key();
            Entry::new(
                ikey.user_key.clone(),
                e.value().clone(),
                ikey.seqno,
                ikey.value_type,
            )
        })
    }

    pub fn approximate_size(&self) -> usize {
        self.approximate_size.load(Ordering::Acquire)
    }

    pub fn highest_seqno(&self) -> SeqNo {
        self.highest_seqno.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

impl Default for Memtable {
    fn default() -> Self {
        Self::new()
    }
}
