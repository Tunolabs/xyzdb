// SPDX-License-Identifier: BUSL-1.1
// unwrap()/expect() are enforced on production code only. Test code — inline
// #[cfg(test)] modules and the integration tests under tests/ — may unwrap
// freely, since a panic there is the failure signal, not a defect. Gating on
// not(test) keeps `cargo clippy --all-targets` on real production debt.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod block;
pub mod bloom;
pub mod cache;
pub mod compaction;
pub mod compression;
pub mod config;
pub mod engine;
pub mod error;
pub mod flush;
pub mod host_probes;
pub mod io;
pub mod journal;
pub mod manifest;
pub mod memory_budget;
pub mod memtable;
pub mod merge;
pub mod merge_op;
pub mod mvcc;
pub mod page_cache;
pub mod snapshot;
pub mod table;
pub mod tree;
pub mod types;
