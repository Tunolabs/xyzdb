// SPDX-License-Identifier: BUSL-1.1
use std::cmp::Ordering;

pub type SeqNo = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ValueType {
    Value = 0,
    Tombstone = 1,
}

impl ValueType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Value),
            1 => Some(Self::Tombstone),
            _ => None,
        }
    }
}

/// Internal key: user_key + seqno + value_type.
///
/// Ordering: user_key ASC, seqno DESC (via Reverse).
/// This ensures that for the same user_key, the highest seqno comes first,
/// which is critical for MVCC correctness — the merge iterator always
/// encounters the newest version before older ones.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InternalKey {
    pub user_key: Vec<u8>,
    pub seqno: SeqNo,
    pub value_type: ValueType,
}

impl InternalKey {
    pub fn new(user_key: Vec<u8>, seqno: SeqNo, value_type: ValueType) -> Self {
        Self {
            user_key,
            seqno,
            value_type,
        }
    }
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.user_key
            .cmp(&other.user_key)
            .then_with(|| other.seqno.cmp(&self.seqno)) // DESC: higher seqno first
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A key-value entry as stored in blocks and memtables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub seqno: SeqNo,
    pub value_type: ValueType,
}

impl Entry {
    pub fn new(key: Vec<u8>, value: Vec<u8>, seqno: SeqNo, value_type: ValueType) -> Self {
        Self {
            key,
            value,
            seqno,
            value_type,
        }
    }

    pub fn internal_key(&self) -> InternalKey {
        InternalKey::new(self.key.clone(), self.seqno, self.value_type)
    }
}

/// Convert a prefix to a half-open range [prefix, upper_bound).
/// For prefix [0x01, 0x02], returns ([0x01, 0x02], [0x01, 0x03]).
/// If prefix ends in 0xFF, truncates until finding an incrementable byte.
/// Returns None for upper bound if the prefix is all 0xFF (scan to end).
pub fn prefix_to_range(prefix: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    let lower = prefix.to_vec();

    // Find the rightmost byte that isn't 0xFF and increment it
    let mut upper = prefix.to_vec();
    while let Some(&last) = upper.last() {
        if last < 0xFF {
            *upper.last_mut().unwrap() += 1;
            return (lower, Some(upper));
        }
        upper.pop();
    }

    // All bytes are 0xFF (or empty prefix) — no upper bound
    (lower, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_key_ordering_user_key_asc() {
        let a = InternalKey::new(vec![1], 10, ValueType::Value);
        let b = InternalKey::new(vec![2], 10, ValueType::Value);
        assert!(a < b);
    }

    #[test]
    fn internal_key_ordering_seqno_desc() {
        let newer = InternalKey::new(vec![1], 20, ValueType::Value);
        let older = InternalKey::new(vec![1], 10, ValueType::Value);
        // Higher seqno should come FIRST (be "less than")
        assert!(newer < older);
    }

    #[test]
    fn internal_key_ordering_same_key_same_seqno() {
        let a = InternalKey::new(vec![1], 10, ValueType::Value);
        let b = InternalKey::new(vec![1], 10, ValueType::Tombstone);
        assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn prefix_to_range_normal() {
        let (lower, upper) = prefix_to_range(&[0x01, 0x02]);
        assert_eq!(lower, vec![0x01, 0x02]);
        assert_eq!(upper, Some(vec![0x01, 0x03]));
    }

    #[test]
    fn prefix_to_range_trailing_ff() {
        let (lower, upper) = prefix_to_range(&[0x01, 0xFF]);
        assert_eq!(lower, vec![0x01, 0xFF]);
        assert_eq!(upper, Some(vec![0x02]));
    }

    #[test]
    fn prefix_to_range_all_ff() {
        let (_, upper) = prefix_to_range(&[0xFF, 0xFF]);
        assert_eq!(upper, None);
    }

    #[test]
    fn prefix_to_range_empty() {
        let (lower, upper) = prefix_to_range(&[]);
        assert!(lower.is_empty());
        assert_eq!(upper, None);
    }
}
