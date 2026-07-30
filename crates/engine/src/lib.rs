// unwrap()/expect() are enforced on production code only. Test code — inline
// #[cfg(test)] modules and the integration tests under tests/ — may unwrap
// freely, since a panic there is the failure signal, not a defect. Gating on
// not(test) keeps `cargo clippy --all-targets` on real production debt.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub(crate) mod aggregate_state;
pub(crate) mod analyze;
pub(crate) mod anchor;
pub(crate) mod cursor;
pub(crate) mod dict_encoding;
pub mod engine;
pub(crate) mod field_registry;
pub(crate) mod ghost;
pub(crate) mod ghost_pool;
pub(crate) mod ghost_router;
pub(crate) mod gravity_spec;
pub mod keyspaces;
pub mod ops;
pub(crate) mod planner;
pub(crate) mod record_cache;
pub(crate) mod reserved_keys;
pub(crate) mod rollup_merge;
pub(crate) mod scan_telemetry;
pub(crate) mod sort_encoding;
pub mod stats;
pub mod throttle;
pub(crate) mod vector_spec;
pub(crate) mod zone_map;

/// Memory-budget resolution (single memory knob → derived block cache).
/// Re-exported from `turba-engine` so `xyzdb-server` can resolve the budget
/// through the engine facade without a direct dependency on the storage crate.
pub use turba_engine::memory_budget;
