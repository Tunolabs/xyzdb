//! Per-key associative merge operator (RocksDB-style), optionally set on a Tree.
//!
//! When a Tree has a merge operator, values written under the SAME key that the
//! operator [`owns`](MergeOperator::owns) are COMBINED rather than resolved
//! last-writer-wins — both at compaction (collapse the version chain into one)
//! and at read (fold the remaining un-compacted chain). Keys the operator does
//! not own keep last-writer-wins, so a Tree that mixes merge keys (e.g. ghost
//! rollups under a reserved prefix) with plain keys (anchors, gravity specs,
//! pins) stays correct.
//!
//! This is what lets ghost rollups be written as blind delta-appends (no
//! read-modify-write): each write appends a partial aggregate under the group's
//! key, compaction folds same-group deltas into one, and a read folds whatever
//! chain remains. See the v0.8 hilo-B design.
//!
//! The combine MUST be associative and commutative: compaction may fold any
//! subset of a key's versions, in any grouping, and a read folds the rest — the
//! result must be independent of how the versions were partitioned or ordered.

/// Combines multiple values for one key into a single value.
pub trait MergeOperator: Send + Sync {
    /// True if this operator combines `key`'s values (vs last-writer-wins).
    /// Must be cheap (e.g. a prefix check): it gates the merge path, so
    /// non-owned keys keep the fast last-writer-wins path with no overhead.
    fn owns(&self, key: &[u8]) -> bool;

    /// Combine an owned key's values into one. `values_newest_first` is ordered
    /// by descending seqno (newest first) and is non-empty. Called only when
    /// [`owns`](Self::owns) returned true. Must be associative + commutative.
    fn merge(&self, key: &[u8], values_newest_first: &[&[u8]]) -> Vec<u8>;
}
