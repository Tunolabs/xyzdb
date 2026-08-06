// SPDX-License-Identifier: BUSL-1.1
use std::sync::Arc;
use turba_engine::config::{
    EngineConfig, IoSchedulerMode as XyzIoSchedulerMode, StorageProfile as XyzStorageProfile,
};
use turba_engine::engine::TurbaEngine;
use turba_engine::journal::writer::PersistMode;
use turba_engine::memory_budget::ResolvedBudget;
use xyzdb_core::error::{Result, XyzError};

/// Default block cache size: 256 MB.
pub const DEFAULT_CACHE_SIZE: u64 = 256 * 1024 * 1024;

/// Storage profile determines HDD-vs-SSD-tuned parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageProfile {
    #[default]
    Ssd,
    Hdd,
}

impl StorageProfile {
    pub fn ghost_block_size(self) -> u32 {
        match self {
            Self::Ssd => 64 * 1024,
            Self::Hdd => 256 * 1024,
        }
    }

    pub fn bloom_bits(self) -> f32 {
        match self {
            Self::Ssd => 10.0,
            Self::Hdd => 14.0,
        }
    }
}

/// I/O scheduler mode (xyzdb-server `--io-scheduler` flag). Mirrors
/// `turba_engine::config::IoSchedulerMode`. Default `Ssd` (Passthrough);
/// `Hdd` opts into the lane-aware scheduler for observability (no
/// throttling — v0.5 retired the enforce ladder per DEC-V5-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IoSchedulerMode {
    #[default]
    Ssd,
    Hdd,
}

/// Open the turba-engine database in single-tier (fintech) mode; data
/// lives under `path`.
#[allow(clippy::too_many_arguments)]
pub fn open_engine(
    path: &str,
    cache_size_bytes: u64,
    manual_journal_persist: bool,
    profile: StorageProfile,
    io_scheduler: IoSchedulerMode,
    l0_batch_override: Option<usize>,
    block_cache_lane_admission: bool,
    wal_path: Option<std::path::PathBuf>,
    memory_budget: ResolvedBudget,
) -> Result<Arc<TurbaEngine>> {
    let config = EngineConfig {
        cache_size_bytes,
        storage_profile: match profile {
            StorageProfile::Ssd => XyzStorageProfile::Ssd,
            StorageProfile::Hdd => XyzStorageProfile::Hdd,
        },
        persist_mode: if manual_journal_persist {
            PersistMode::Buffer
        } else {
            PersistMode::SyncData
        },
        io_scheduler: match io_scheduler {
            IoSchedulerMode::Ssd => XyzIoSchedulerMode::Ssd,
            IoSchedulerMode::Hdd => XyzIoSchedulerMode::Hdd,
        },
        l0_batch_override,
        block_cache_lane_admission,
        wal_path,
        wal_segment_max_bytes: turba_engine::journal::writer::DEFAULT_SEGMENT_MAX_BYTES,
        memory_budget_bytes: memory_budget.bytes,
        budget_source: memory_budget.source,
    };

    let path = std::path::Path::new(path);
    let engine = TurbaEngine::open(path, config).map_err(|e| {
        XyzError::Storage(format!(
            "failed to open turba-engine at '{}': {e}",
            path.display()
        ))
    })?;
    Ok(Arc::new(engine))
}
