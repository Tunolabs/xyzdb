//! Serialisable snapshot of engine-wide health metrics. Consumed by the
//! `STATS` short-circuit handler in the server to produce a JSON response
//! without materialising through the tracing/log pipeline.
//!
//! Stable-ish schema: consumers (dashboards, scrapers) can rely on the
//! top-level keys staying put, but fields MAY be added over time. Breaking
//! rename/removal goes behind a schema-version bump.

use serde::Serialize;
use std::collections::BTreeMap;

/// Top-level /stats response body.
#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub keyspaces: BTreeMap<String, KeyspaceStats>,
    pub block_cache: BlockCacheStats,
    pub ghosts: GhostStats,
    /// #11 — per-lobe keel-omit health, one entry per gravity-declared lobe
    /// (empty when no lobe declares a gravity spec). Diagnostic only: placement
    /// is unaffected; this surfaces silent scoped-recall degradation when writes
    /// omit the declared gravity field (case C).
    pub keel_health: Vec<KeelHealthEntry>,
    pub sync_thread: SyncThreadStats,
    pub process: ProcessStats,
    pub cgroup: CgroupStats,
    /// I/O scheduler metrics. Always present; for `Scheduler::Passthrough`
    /// (default `--io-scheduler ssd`) all per-lane values are zero
    /// because Passthrough never accumulates. v0.3-cycle Spike A.3.
    pub scheduler: SchedulerStats,
    /// Per-component RAM accounting + ratio against `process.vmrss_bytes`.
    /// Pure observability; no enforcement.
    pub ram_budget: RamBudgetSnapshot,
}

/// Per-component RAM accounting (observability only, v0.6.0-pre). Each
/// field is a best-effort estimate of bytes currently held by that
/// component; `total_estimated_bytes` is the sum, `vmrss_bytes` is the
/// OS-level resident-set size from `/proc/self/status`, and `ratio`
/// is `total_estimated_bytes / vmrss_bytes` (1.0 = perfect coverage;
/// < 1.0 = un-accounted RAM in the process; > 1.0 = double-counting
/// or stale measurements).
#[derive(Debug, Serialize)]
pub struct RamBudgetSnapshot {
    /// BlockCache `current_weight()` (bytes admitted to quick_cache).
    pub block_cache_bytes: u64,
    /// RecordCache `used_bytes()` (sum of `Record::estimated_size`).
    /// 0 when RecordCache is not enabled at server boot.
    pub record_cache_bytes: u64,
    /// Memtables (active + sealed) across all five keyspaces. Sums
    /// `active_memtable_size + sealed_memtable_bytes` from each tree.
    pub memtables_bytes: u64,
    /// Sum of SST metadata bytes per tree: bloom + index + zone_map
    /// (from `TreeMemoryBreakdown::*_per_level` vectors).
    pub sst_metadata_bytes: u64,
    /// Best-effort estimate of registry-resident bytes (LobeRegistry,
    /// AnchorRegistry, LobeFieldRegistry, DictRegistry,
    /// ScanTelemetryRegistry, GhostLobeManager). v0.6.0-pre populates
    /// this as 0 — registries are typically <1% of VmRSS and the precise
    /// accounting is deferred. The field is reserved so consumers see a
    /// stable schema.
    pub registries_bytes: u64,
    /// In-RAM ghost aggregate state: every ghost's `global_aggregates`
    /// plus its `group_summaries` map (0.7.5). High-cardinality GROUP BY
    /// ghosts make this gigabytes at scale — leaving it out of the model
    /// was the main reason `ratio` under-reported (0.41 measured on the
    /// scale-1 bench while VmRSS sat at ~4.9 GB).
    pub ghost_aggregates_bytes: u64,
    /// Sum of the per-component fields above.
    pub total_estimated_bytes: u64,
    /// Resident-set size from `/proc/self/status` (Linux). 0 if the OS
    /// probe is unavailable.
    pub vmrss_bytes: u64,
    /// `total_estimated_bytes as f64 / vmrss_bytes as f64`. 0.0 when
    /// `vmrss_bytes == 0`. Soak gate target is `[0.85, 1.15]` under the
    /// canonical workload.
    pub ratio: f64,
}

/// Top-level scheduler health surface. Mirrors the I/O lanes defined in
/// `turba_engine::io::Lane`. Indexed by lane name (stable): `user_io_read`,
/// `writer_durable`, `flush`, `compaction`, `scan`.
#[derive(Debug, Serialize)]
pub struct SchedulerStats {
    /// `"passthrough"` or `"laned"`. Reflects the variant active on the
    /// running engine; flipped via `--io-scheduler {ssd,hdd}` at open.
    pub mode: String,
    pub user_io_read: LaneStats,
    pub writer_durable: LaneStats,
    pub flush: LaneStats,
    pub compaction: LaneStats,
    /// Bulk scans routed off UserIORead because their span exceeds the
    /// block-cache capacity (0.9 G2). 0 under Passthrough / when no such
    /// scan has run.
    pub scan: LaneStats,
    /// Cumulative cross-lane outstanding peak — peak observed sum of
    /// in-flight ops across all four lanes. Captures kernel-level
    /// disk-queue saturation that per-lane peaks may miss when peaks
    /// stagger in time. 0 under Passthrough. Added in v0.3.2 Spike B.
    /// Cumulative over the
    /// scheduler's lifetime; pre/post-phase snapshot deltas approximate
    /// phase-local maxima alongside the histogram + miss-count deltas.
    pub cross_lane_outstanding_peak: u32,
}

#[derive(Debug, Serialize)]
pub struct LaneStats {
    /// `after_op` calls observed on this lane (sum of completed ops).
    pub ops: u64,
    /// Sum of `elapsed_us` reported across `after_op` calls.
    pub elapsed_us_total: u64,
    /// Convenience: `elapsed_us_total / ops` when ops > 0, else 0.0.
    pub avg_elapsed_us: f64,
    /// Currently in-flight ops (`before_op` minus `after_op`). Clamped
    /// to 0 if the snapshot saw a brief negative race.
    pub outstanding: u32,
    /// Peak observed `outstanding` since engine open. Used to tune the
    /// bounded-outstanding ladder in H1.
    pub outstanding_peak: u32,
    /// Sliding-window P50 service time (µs) over the last 1 s on this
    /// lane. 0 if the window is empty or the scheduler is Passthrough.
    pub p50_us: u64,
    /// EWMA of `p50_us` updates on this lane (µs). α = 0.3. Smooths
    /// transient spikes seen by the bounded-outstanding ladder consumer
    /// in H1.3. 0 until at least one `p50_us` query has produced a value.
    pub ewma_p50_us: u64,
    /// Cumulative count of `current_p50 > slo_us` events on this lane.
    /// Stays at 0 in H1.2 observe-only (`slo_us = u64::MAX`); H1.3
    /// supplies real SLO values and surfaces real breach activity.
    pub slo_breach_count: u64,
}

/// Group-commit sync thread health. Exposed so operators can detect a
/// stalled or dead sync thread under write load: a flat
/// `last_successful_sync_ts_ms` while writes are happening means writers
/// are blocked on a broken durability path (see Finding 9).
/// `heartbeat_count` is a liveness counter independent of sync success:
/// heartbeat climbing while `last_successful_sync_ts_ms` stays flat
/// means the thread is alive but every fsync is failing.
#[derive(Debug, Serialize)]
pub struct SyncThreadStats {
    /// Unix epoch milliseconds. 0 if no sync has ever succeeded — either
    /// group-commit is disabled (Batched/Async durability modes) or the
    /// thread has not yet completed a successful cycle.
    pub last_successful_sync_ts_ms: u64,
    /// Always 0 when group-commit is disabled.
    pub heartbeat_count: u64,
}

#[derive(Debug, Serialize)]
pub struct KeyspaceStats {
    /// L0..L6 table counts. Keys are `l0`..`l6` for stable schema.
    pub levels: BTreeMap<String, usize>,
    /// Sum of `levels.values()`. Convenience; derivable from `levels`.
    pub version_sum: usize,
    /// On-disk SSTable file count, may exceed `version_sum` transiently
    /// while old inputs are being unlinked after a compaction publish.
    pub disk_sst: usize,
    /// Highest seqno that has been flushed to SSTables in this tree.
    pub flushed_seqno: u64,
    pub memory: MemoryStats,
    pub compact: CompactStats,
    /// Per-keyspace block-cache attribution. Distinguishes cache hits from
    /// disk-bound misses and accumulates the wall-clock time spent on each
    /// path. Sum of per-tree `hits` across all keyspaces equals the global
    /// `BlockCacheStats.hits`; same for `misses`. Added in v0.3 cycle Day
    /// 0-1 (Spike 0 ampliado) — required to attribute Q1 latency to cache
    /// miss vs disk service per the cycle's gating analysis.
    pub block_cache: PerTreeBlockCacheStats,
    /// OS page-cache residency aggregated across this keyspace's SSTables.
    /// Linux-only; zero on macOS / other targets. Added v0.3 cycle Day 2-3.
    pub page_cache: PageCacheStats,
    /// Telemetry from the eager manifest-replay loop at engine open. Reports
    /// the wall time, total bloom + index + meta bytes loaded, and SSTable
    /// count. Write-once at engine open; values do not change post-boot.
    pub warmup: WarmupStats,
}

/// Eager-warmup telemetry per keyspace. Mirrors
/// `turba_engine::tree::WarmupStats`. The warmup itself was already happening
/// pre-H1.1 (each `SSTableReader::open_with_tree_id` reads bloom + index +
/// meta synchronously); H1.1 just measures it. A future regression that
/// introduces lazy bloom loading will show `bytes_loaded` diverging from the
/// expected per-SSTable bloom + index + meta sum.
#[derive(Debug, Serialize)]
pub struct WarmupStats {
    /// Wall time of the manifest-replay loop, in milliseconds.
    pub wall_ms: u64,
    /// Sum of bloom + index + meta bytes across every opened SSTable.
    pub bytes_loaded: u64,
    /// Count of SSTables opened (manifest entries whose path existed on disk).
    pub sstables_opened: usize,
}

#[derive(Debug, Serialize)]
pub struct PerTreeBlockCacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Microseconds accumulated on cache-miss paths (loader closure ran).
    pub disk_read_us_total: u64,
    /// Microseconds accumulated on cache-hit paths (no disk I/O).
    pub cache_read_us_total: u64,
    /// Convenience: `disk_read_us_total / misses` when misses > 0, else 0.0.
    pub avg_disk_read_us: f64,
    /// Convenience: `cache_read_us_total / hits` when hits > 0, else 0.0.
    pub avg_cache_read_us: f64,
    /// Cumulative `pread` service-time histogram, 10 buckets aligned with
    /// HDD physics. Index → range:
    /// `[<1, 1-3, 3-5, 5-8, 8-12, 12-20, 20-50, 50-100, 100-300, ≥300] ms`.
    /// Surfaced as JSON array. Spike B (v0.3.2) uses pre/post-phase
    /// snapshot deltas to derive phase-local distributions.
    pub pread_service_time_us_histogram: [u64; 10],
}

/// OS page-cache residency aggregated across this keyspace's SSTables.
/// All zeros on non-Linux targets (the syscall is gated to Linux to avoid
/// reporting OrbStack-VM-mediated numbers that mislead operators).
/// Surfaced in v0.3 cycle Day 2-3 — separates block-cache misses that
/// actually hit OS page cache (~10 µs) from misses that pay full disk
/// service time (~10 ms HDD seek).
#[derive(Debug, Serialize)]
pub struct PageCacheStats {
    pub resident_pages: u64,
    pub total_pages: u64,
    pub file_size_bytes: u64,
    /// Resident fraction in `[0.0, 1.0]`.
    pub residency_ratio: f64,
}

#[derive(Debug, Serialize)]
pub struct MemoryStats {
    pub mem_active_bytes: u64,
    pub sealed_count: u64,
    pub sealed_bytes: u64,
    pub zone_maps_bytes: u64,
    pub index_bytes: u64,
    pub bloom_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct CompactStats {
    /// Background `maybe_compact` cycles that returned Ok(true).
    pub compact_ok: u64,
    /// Successful `major_compact_with_observer` invocations. See Finding 7
    /// Bug B1 — separable from `compact_ok` so operators can distinguish
    /// "bg throughput" from "manual bulk consolidation".
    pub major_ok: u64,
    /// Failed background compact cycles. Major compact failures propagate
    /// to the caller and are NOT counted here — see Tree docstring.
    pub compact_err: u64,
    /// Number of H2.1 trivial-moves applied — single-input no-target-overlap
    /// compactions resolved by a manifest-only update (no rewrite). 0 in
    /// pre-H2.1 builds.
    pub trivial_move_count: u64,
    /// Sum of `input.meta().file_size` across applied trivial-moves —
    /// the bytes that would have been re-written under non-trivial
    /// compaction. Empirical sizing of the H2.1 savings. 0 in pre-H2.1.
    pub trivial_move_bytes_saved: u64,
    /// Count of H2.2 pre-warm L0 invocations — increments once per
    /// `major_compact_with_observer` call that has at least one L0 input.
    /// See §9.2. 0 in pre-H2.2 builds.
    pub prewarm_l0_invocations: u64,
    /// Total bytes read by pre-warm sequential `std::io::copy` calls.
    /// Approximately Σ file_size of L0 inputs across invocations.
    pub prewarm_l0_bytes_read: u64,
    /// Cumulative wall-clock microseconds in pre-warm.
    pub prewarm_l0_wall_us: u64,
    /// Per-file pre-warm errors (soft-fail; never propagated). A
    /// climbing counter without a climbing wall is an operational signal.
    pub prewarm_l0_errors: u64,
}

#[derive(Debug, Serialize)]
pub struct BlockCacheStats {
    pub weight_bytes: u64,
    pub capacity_bytes: u64,
    pub len: u64,
    pub hits: u64,
    pub misses: u64,
    /// v0.4 cp 4.2.1: BlockCache lane-aware admission counters. The
    /// `admission` array is indexed by `Lane::index()` (0=UserIORead,
    /// 1=WriterDurable, 2=Flush, 3=Compaction, 4=Scan). Each entry reports
    /// the `admitted` and `skipped` totals on miss for that lane since
    /// engine open. With `lane_admission_enabled = false`, every miss
    /// admits and `skipped` stays at 0. Scan (0.9 G2) reports `skipped`
    /// for bulk sweeps whose span exceeds the cache capacity.
    pub lane_admission_enabled: bool,
    pub admission: [LaneAdmissionStats; turba_engine::io::Lane::COUNT],
}

#[derive(Debug, Serialize, Default, Clone, Copy)]
pub struct LaneAdmissionStats {
    /// Misses that proceeded to insert into the cache. v0.4 cp 4.2.1.
    pub admitted: u64,
    /// Misses for this lane that the policy declined to admit.
    /// Always 0 for UserIORead / WriterDurable; populated for
    /// Flush / Compaction when `lane_admission_enabled` is true.
    pub skipped: u64,
}

#[derive(Debug, Serialize)]
pub struct GhostStats {
    pub total: u64,
    pub per_lobe: Vec<GhostLobeEntry>,
    /// Auto-promotion telemetry. Pre-design instrumentation for the
    /// v0.3.2-ghost-singleflight cycle: Spike D pinned 74.4 % of 8R CPU on
    /// `Engine::maybe_create_ephemeral_ghost` (vs 0 % at 1R), and the cycle
    /// fix is a single-flight + bounded-pool wrapper. These counters quantify
    /// the mechanism narrative pre-fix (spawn rate, dedup-loss rate) and the
    /// fix delta post-fix (skip rate, spawn rate decay) without relying on
    /// flamegraph diff alone.
    pub auto: GhostAutoStats,
}

/// Auto-promotion telemetry counters (cumulative since engine open).
///
/// Pre-fix invariant: `candidate_total ≈ candidate_spawn` (every candidate
/// reaches the spawn site; dedup happens post-spawn, inside
/// `GhostLobeManager::create`). `dedup_lost` is the share of those spawns
/// that lost the race and did discarded scan + partial build.
/// `singleflight_skipped` stays 0 — the field exists for schema stability
/// across the v0.3.2-ghost-singleflight transition.
///
/// Post-fix invariant: `candidate_total ≈ candidate_spawn + singleflight_skipped`,
/// and `dedup_lost ≈ 0` (single-flight catches duplicates before the spawn).
/// The pre→post delta on `candidate_spawn` is the empirical proof of the
/// mechanism class verdict.
#[derive(Debug, Serialize)]
pub struct GhostAutoStats {
    /// Total `Engine::maybe_create_ephemeral_ghost` invocations evaluated
    /// (every candidate from scan telemetry, regardless of subsequent
    /// spawn / skip / abort decision). Increment site is the first line of
    /// the function, before any early-return guards (lobe lookup, weak_self
    /// presence, etc.) so the count tracks "candidates the path saw" rather
    /// than "candidates that survived prelude checks".
    pub candidate_total: u64,
    /// Subset of `candidate_total` that proceeded to
    /// `std::thread::Builder::new().spawn(...)`. Pre-fix: ≈ candidate_total
    /// minus the early-return guards. Post-fix:
    /// `candidate_total - singleflight_skipped` minus the same guards.
    pub candidate_spawn: u64,
    /// Subset of `candidate_spawn` whose `GhostLobeManager::create` returned
    /// `Err(XyzError::GhostExists(_))` — the thread did discarded scan +
    /// partial build before the in-create dedup rejected the duplicate.
    /// This is the slice the v0.3.2-ghost-singleflight cycle aims to drive
    /// to ≈ 0.
    pub dedup_lost: u64,
    /// Post-fix only: candidates short-circuited at the single-flight entry
    /// (`DashSet::insert` returned false) before reaching the spawn site.
    /// Stays 0 in the pre-fix build; populated when the single-flight path
    /// lands.
    pub singleflight_skipped: u64,
    /// Subset of `candidate_spawn` whose `GhostLobeManager::create` returned
    /// `Err(_)` for any reason OTHER than `XyzError::GhostExists` (Storage
    /// errors, dictionary contention, bloom-filter rebuild races, etc).
    /// These are spawned threads that paid the scan + partial build cost
    /// AND still failed to register a ghost — i.e. work entirely wasted,
    /// for a reason that isn't the dedup race the cycle's single-flight
    /// targets. Captured separately from `dedup_lost` so the design doc
    /// can quantify how much of the 74 % CPU bill is "race losers" vs
    /// "errors that single-flight wouldn't catch".
    pub create_failed_other: u64,
    /// Candidates dropped at the bounded-pool entry: `submit` returned
    /// `false` because either the channel backlog (`N * 4` slots) was
    /// full or the sender was already disconnected (engine drop in
    /// flight). Combines both failure modes because Disconnected only
    /// occurs at shutdown — not a runtime production signal — so a
    /// single counter preserves diagnostic value without splitting.
    /// Sustained non-zero values during steady-state traffic indicate
    /// the pool is undersized for the candidate firing rate.
    pub pool_submit_failed: u64,
}

#[derive(Debug, Serialize)]
pub struct GhostLobeEntry {
    /// Ghost lobe name (e.g., "items_topn_by_amount").
    pub name: String,
    /// Source lobe (e.g., "items").
    pub source_lobe: String,
    pub record_count: u64,
}

/// #11 — per-lobe keel (gravity-field) omit health. One entry per lobe that has
/// a declared gravity spec. The denominator is gravity-declared PUTs only, so
/// `omit_ratio` is not diluted by lobes without gravity. Additive observability:
/// a PUT that omits the declared field still lands (anchor/LID fallback) and
/// stays recoverable by an unfiltered `SCAN`; it is simply not co-located and is
/// (correctly) excluded from `WHERE <field> = X`. A rising ratio means scoped
/// queries silently under-recall — the writer is dropping the keel.
#[derive(Debug, Serialize)]
pub struct KeelHealthEntry {
    /// Lobe with a declared gravity spec.
    pub lobe: String,
    /// PUTs whose record carried the declared gravity field (co-located).
    pub keel_present: u64,
    /// PUTs that omitted it (case C): placed on the anchor/LID fallback bucket,
    /// not co-located. A record with an anchor but no gravity field co-locates by
    /// the anchor axis, not the declared keel — counted here too (misplacement).
    pub keel_absent: u64,
    /// `keel_absent / (keel_present + keel_absent)`; `0.0` when no samples yet.
    pub omit_ratio: f64,
}

/// Process-level metrics. Linux-only — all fields are 0 on macOS / other
/// platforms. Raw bytes (converted from MB internally; loss of sub-MB
/// precision is acceptable for a health endpoint).
#[derive(Debug, Serialize)]
pub struct ProcessStats {
    pub vmrss_bytes: u64,
    pub vmdata_bytes: u64,
}

/// Cgroup v1/v2 memory accounting. Linux-only — 0 on macOS.
#[derive(Debug, Serialize)]
pub struct CgroupStats {
    pub anon_bytes: u64,
    pub file_bytes: u64,
    pub active_file_bytes: u64,
    pub inactive_file_bytes: u64,
}
