//! Engine-level configuration.

use std::path::PathBuf;

use crate::journal::writer::PersistMode;

/// Storage profile: tunes block sizes and bloom filters for disk type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageProfile {
    Ssd,
    Hdd,
}

/// I/O scheduler mode (cycle doc §6 D6). Independent from
/// [`StorageProfile`] — operators may run an SSD-tuned engine on a
/// rotational disk and choose the scheduler explicitly. Default `Ssd`
/// (Passthrough); `Hdd` opts into the lane-aware scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoSchedulerMode {
    /// Zero-overhead passthrough. Default. v0.3-cycle Spike A semantics:
    /// no observation, no throttling — std::fs path runs as-is.
    Ssd,
    /// Lane-aware scheduler. Spike A.3 ships the observe-only shell;
    /// H1 (Day 16-25) wires the bounded-outstanding ladder + token
    /// buckets + service-time backoff.
    Hdd,
}

/// Top-level engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub cache_size_bytes: u64,
    pub storage_profile: StorageProfile,
    pub persist_mode: PersistMode,
    pub worker_threads: usize,
    /// I/O scheduler mode. Default Ssd (Passthrough). Cycle doc §6 D6.
    pub io_scheduler: IoSchedulerMode,
    /// Optional override for `LeveledConfig.l0_compact_batch_size`.
    /// `None` = use the storage-profile default from
    /// `LeveledConfig::for_storage_profile`. `Some(n)` = override to `n`.
    /// Surfaced via xyzdb-server's `--l0-batch` advanced-tuning flag for
    /// operators + the H2.3 sweep protocol (see cycle doc §9.3).
    pub l0_batch_override: Option<usize>,
    /// v0.4 cp 4.2.1: when true, the BlockCache uses lane-aware
    /// admission — Compaction + Flush block-misses do NOT insert.
    /// Toggled via `--block-cache-lane-admission {enabled, disabled}`.
    pub block_cache_lane_admission: bool,
    /// v0.5.2: optional override for the WAL location. `None` (default)
    /// keeps the historical `<path>/journal.wal` co-located with the
    /// data directory; `Some(p)` places the WAL at `p` and the rest of
    /// the data dir (SSTs, MANIFEST, snapshots) at `path`. Surfaced via
    /// `xyzdb-server --wal-path`. The two paths should share a
    /// filesystem (hard-link orchestration for snapshots assumes it).
    pub wal_path: Option<PathBuf>,
    /// Size at which the active WAL segment rolls to an archived
    /// `journal.<seqno>.wal`. Archived segments fully below the manifest-durable
    /// watermark are pruned automatically in the background, bounding the WAL to
    /// roughly a couple of segments instead of the full write history. Default
    /// `DEFAULT_SEGMENT_MAX_BYTES` (64 MB); tests use a small value to force rolls.
    pub wal_segment_max_bytes: u64,
    /// The single memory knob: the total memory budget in bytes from which
    /// `cache_size_bytes` is derived (a quarter, floored at 32 MiB — see
    /// [`crate::memory_budget::cache_bytes_from_budget`]). Informational at
    /// this layer; the load-bearing derived value is `cache_size_bytes`.
    /// Default [`crate::memory_budget::DEFAULT_BUDGET_BYTES`] (1 GiB), which
    /// derives to the historical 256 MB cache.
    pub memory_budget_bytes: u64,
    /// Where `memory_budget_bytes` came from (explicit override, cgroup
    /// auto-detect, or the conservative default). Set by the resolver in
    /// `xyzdb-server`; defaults to [`crate::memory_budget::BudgetSource::Default`].
    pub budget_source: crate::memory_budget::BudgetSource,
}

/// Validation error surfaced by `EngineConfig::validate` at startup.
/// Distinct error type so `xyzdb-server` can produce a clear operator
/// message before the engine even attempts to open.
///
/// Single-tier (fintech) configuration has no validation failure modes,
/// so this enum currently carries no variants; it remains as the
/// `validate` return type for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

impl std::error::Error for ConfigError {}

impl EngineConfig {
    /// Validate the configuration. Called by `xyzdb-server` before
    /// `Engine::open` to surface operator-friendly errors. The engine
    /// open path also calls this defensively. The single-tier
    /// configuration has no failure modes, so this always returns `Ok`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            cache_size_bytes: 256 * 1024 * 1024, // 256MB
            storage_profile: StorageProfile::Ssd,
            persist_mode: PersistMode::Buffer,
            worker_threads: 2,
            io_scheduler: IoSchedulerMode::Ssd,
            l0_batch_override: None,
            block_cache_lane_admission: true,
            wal_path: None,
            wal_segment_max_bytes: crate::journal::writer::DEFAULT_SEGMENT_MAX_BYTES,
            memory_budget_bytes: crate::memory_budget::DEFAULT_BUDGET_BYTES,
            budget_source: crate::memory_budget::BudgetSource::Default,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        let c = EngineConfig::default();
        c.validate().unwrap();
    }
}
