// SPDX-License-Identifier: BUSL-1.1
use crate::anchor::AnchorRegistry;
use crate::dict_encoding::DictRegistry;
use crate::field_registry::LobeFieldRegistry;
use crate::ghost::GhostLobeManager;
use crate::ghost_router::GhostRouter;
use crate::gravity_spec::GravitySpec;
use crate::record_cache::RecordCache;
use crate::scan_telemetry::ScanTelemetryRegistry;
use crate::throttle::WriteThrottle;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use xytalk_parser::ast::Statement;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::lobe::LobeRegistry;
use xyzdb_core::record::Record;
pub use xyzdb_core::result::QueryResult;

/// Reserved dictionary prefix for pinned fields: `[PIN][lobe_id:2]`, value
/// `[MAGIC][0x01][postcard(Vec<String>)]`.
///
/// The prefix has moved twice as earlier collisions were found, each time
/// onto another live keyspace: `[0xFF,0xFD]` (ghost metadata) pre-0.7.6, then
/// `[0xFF,0xFB]` (field registry) in 0.7.6. It now lives at a prefix owned by
/// nobody else; the canonical value and the build-enforced non-collision
/// invariant are in [`crate::reserved_keys`]. `load_pinned_fields` still reads
/// [`PIN_PREFIX_LEGACY`] as a boot-time fallback — accepting only pin-shaped
/// values — and migrates them here.
const PIN_PREFIX: [u8; 2] = crate::reserved_keys::PIN;

/// Pre-0.7.6 pin prefix, shared with ghost metadata (disambiguated by the
/// value format byte). Read-only migration fallback; see [`PIN_PREFIX`] and
/// [`crate::reserved_keys::PIN_LEGACY`].
const PIN_PREFIX_LEGACY: [u8; 2] = crate::reserved_keys::PIN_LEGACY;

/// Reserved dictionary prefix for the per-lobe gravity-field registry
/// (Finding 13): `[GRAVITY][lobe_id:2]`. Stores the gravity field name
/// observed on the first PUT to a lobe, so SCAN can use it as a primary
/// index in the equality fast path. See `ops/scan.rs` and
/// [`crate::reserved_keys::GRAVITY`].
const GRAVITY_PREFIX: [u8; 2] = crate::reserved_keys::GRAVITY;

/// Reserved dictionary key for the global per-open boot epoch (2a). Read,
/// incremented, and durably persisted on every `open` before any LID is
/// minted; the value (u16 BE) is embedded in each LID's low 16 bits so LIDs
/// from different opens never collide even if the wall clock repeats a
/// microsecond and the in-process LID sequence has reset. The key is exactly
/// these two bytes — see [`crate::reserved_keys::BOOT_EPOCH`].
const BOOT_EPOCH_KEY: [u8; 2] = crate::reserved_keys::BOOT_EPOCH;

/// 2b — number of anchor write-serialization shards (see `anchor_shard_locks`).
/// Keyed by `dict_key` hash; a power of two keeps the modulo cheap and the
/// false-sharing of unrelated anchors negligible at this count.
const ANCHOR_SHARDS: usize = 256;

/// V3: Durability mode for writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DurabilityMode {
    /// Every write is fsynced. Zero data loss on crash. ~100 IOPS on HDD.
    #[default]
    Durable,
    /// Fsync every N ms. Loses last batch on crash. ~10× throughput on HDD.
    Batched,
    /// OS decides when to flush (~30s). Maximum speed. Up to 30s of data lost on crash.
    Async,
}

/// #11 — per-lobe keel (gravity-field) omit counters. Only lobes with a declared
/// gravity spec accumulate here, so the denominator is gravity-declared PUTs and
/// the omit ratio is not diluted by lobes without gravity. Purely additive
/// observability: placement is unchanged; a PUT that omits the declared keel still
/// lands (anchor/LID fallback) and stays SCAN-recoverable — this only counts it so
/// a scoped-recall regression is visible instead of silent.
#[derive(Default)]
pub(crate) struct KeelHealthCounters {
    /// PUTs whose record carried the declared gravity field (co-located on the keel).
    pub(crate) present: std::sync::atomic::AtomicU64,
    /// PUTs to a gravity-declared lobe that OMITTED the field (case C).
    pub(crate) absent: std::sync::atomic::AtomicU64,
    /// Latched once the omit ratio first crosses the warn threshold (warn-once).
    pub(crate) warned: std::sync::atomic::AtomicBool,
}

/// The xyzDB engine. Owns the Turba storage engine and all registries.
pub struct Engine {
    pub(crate) turba: std::sync::Arc<turba_engine::engine::TurbaEngine>,
    pub(crate) lobe_registry: RwLock<LobeRegistry>,
    pub(crate) anchor_registry: RwLock<AnchorRegistry>,
    pub(crate) ghost_manager: GhostLobeManager,
    pub throttle: WriteThrottle,
    /// Ghost routers: per-lobe routing decisions (primary vs ghost).
    pub(crate) ghost_routers: RwLock<HashMap<u16, GhostRouter>>,
    /// Telemetry for scan operations.
    pub(crate) scan_telemetry: RwLock<ScanTelemetryRegistry>,
    /// V3: Pinned fields per lobe (lobe_name → Vec<field_name>).
    pub(crate) pinned_fields: RwLock<HashMap<String, Vec<String>>>,
    /// Finding 13 / v0.8 keel: per-lobe gravity spec (lobe_name → GravitySpec).
    /// Populated on first PUT to a lobe (as `Raw(field)`) or by `GRAVITY BY`;
    /// consulted by both placement (WRITE) and the SCAN equality fast path
    /// (QUERY) through the same value so they cannot diverge. Persisted in the
    /// dictionary keyspace under `GRAVITY_PREFIX`; pre-0.8 records (a bare
    /// field name) decode to `Raw` with zero migration.
    pub(crate) gravity_specs: RwLock<HashMap<String, GravitySpec>>,
    /// #11 — per-lobe keel-omit health (lobe_name → counters), populated only for
    /// lobes with a declared gravity spec. See [`KeelHealthCounters`]. Diagnostic.
    pub(crate) keel_health: RwLock<HashMap<String, KeelHealthCounters>>,
    /// #11 — omit ratio at which a gravity-declared lobe first warns (default
    /// 0.01 = 1%; env `XYZDB_KEEL_OMIT_WARN_RATIO`, clamped to [0, 1]). Diagnostic.
    pub(crate) keel_omit_warn_ratio: f64,
    /// v0.8 keel sibling axis: per-lobe searchable vector field
    /// (lobe_name → VectorSpec). Declared by `VECTOR <field> IN "lobe"` before
    /// the first write; consulted later by PUT to hoist the named embedding to
    /// the V3 record prefix for exact NEAREST. Persisted in the dictionary
    /// keyspace under `reserved_keys::VECTOR_FIELD`. NOT an index/IVF.
    pub(crate) vector_fields: RwLock<HashMap<String, crate::vector_spec::VectorSpec>>,
    /// Sub-gravity axis (third sibling to gravity/vector): per-lobe satellite
    /// field (lobe_name → SatelliteSpec). Declared by `SATELLITE BY <field> IN
    /// "lobe"` on an EMPTY lobe; names the field whose value sub-buckets the
    /// gravity bucket via the reserved `sat` axis of the spatial key. Persisted
    /// under `reserved_keys::SATELLITE`. Declaration-only in this phase — write
    /// placement (`hash16` into `sat`) and bounded reads are a later phase, so
    /// today a declared spec is inert (every record still lands in sat 0).
    pub(crate) satellite_specs: RwLock<HashMap<String, crate::satellite_spec::SatelliteSpec>>,
    /// D1: set at open when any persisted gravity slot is pre-D1 (name+value,
    /// format byte 0x01/0x02). While true the engine refuses gravity data ops
    /// (PUT/SCAN/FIND/…) with a "run migrate" error, so it never silently reads
    /// name+value-placed records through the value-only fast path. `migrate`
    /// rehashes the keys, re-persists every spec at 0x03, and clears this.
    pub(crate) gravity_needs_migration: std::sync::atomic::AtomicBool,
    /// V3: Dictionary encoding store (lobe+field → codec).
    pub(crate) dict_store: RwLock<DictRegistry>,
    /// V5: Field name → u16 ID mapping per lobe (for V2 on-disk format).
    pub(crate) field_registry: RwLock<LobeFieldRegistry>,
    /// V5: Explicit in-memory cache for hot data (None if --record-cache-size=0).
    pub(crate) record_cache: Option<RecordCache>,
    /// M2.2 airbag: a `NEAREST` bucket scan past this many ms aborts with
    /// [`XyzError::NearestBudgetExceeded`] instead of hanging silently now that
    /// `NEAREST` is decoupled from the SCAN cap (M2.1). `0` disables.
    ///
    /// Default 3000 is CALIBRATED, not a placeholder: the worst dimensioned-for
    /// bucket (1536d / 250k, the fused path at 1 core) measures p99 ≈ 1505ms and
    /// a tail max ≈ 2502ms. 3000 = 2× the p99, clears the observed max with ~20%
    /// margin, and absorbs the OrbStack-proxy → real-T6-x86 gap. 250k is the
    /// dimensioned ceiling (it must PASS, not abort); a runaway (1M+ bucket, >3s)
    /// still trips it. Fixed for T6 — a per-CPU-derived budget stays parked until
    /// there is data from more than one box. Set via `--nearest-budget-ms`.
    pub(crate) nearest_budget_ms: u64,
    /// V3: Durability mode.
    durability: DurabilityMode,
    meta_path: PathBuf,
    /// Weak back-reference to the `Arc<Engine>` the outside world holds.
    /// Set by `into_arc()`; consumed by methods that need to spawn a thread
    /// holding a stable reference (auto-ghost creation, the TTL reaper,
    /// promotion). Remains `None` if the caller never
    /// wrapped the engine in `Arc`, which is fine for single-threaded tests
    /// and CLIs that never invoke the lifecycle paths.
    pub(crate) weak_self: std::sync::OnceLock<std::sync::Weak<Engine>>,

    /// Auto-promotion telemetry counters. See `crate::stats::GhostAutoStats`
    /// for invariants and the v0.3.2-ghost-singleflight pre-fix vs post-fix
    /// contract. Cumulative since engine open; reset at process restart.
    pub(crate) ghost_candidate_total_count: std::sync::atomic::AtomicU64,
    pub(crate) ghost_candidate_spawn_count: std::sync::atomic::AtomicU64,
    pub(crate) ghost_dedup_lost_count: std::sync::atomic::AtomicU64,
    pub(crate) ghost_singleflight_skipped_count: std::sync::atomic::AtomicU64,
    pub(crate) ghost_create_failed_other_count: std::sync::atomic::AtomicU64,
    pub(crate) ghost_pool_submit_failed_count: std::sync::atomic::AtomicU64,

    /// Bounded pool for ephemeral ghost creation. Wired into
    /// `maybe_create_ephemeral_ghost`.
    pub(crate) ghost_pool: crate::ghost_pool::GhostCreatorPool,

    /// In-flight set for the single-flight layer (PASO 6.3).
    /// Members are `xxh3_64(filter_desc)` values for candidates whose
    /// pool submit succeeded and whose worker has not yet finished.
    /// `Engine::maybe_create_ephemeral_ghost` calls
    /// `ghost_inflight.insert(hash)`; if the call returns `false`
    /// (entry already present), the candidate is skipped. The hash is
    /// removed when the corresponding `GhostCreateJob`'s
    /// `SingleflightGuard` drops — see `ghost_pool::SingleflightGuard`
    /// for the panic-safe RAII contract. `Arc` so the guard can hold
    /// a clone without lifetime constraints on the Engine.
    pub(crate) ghost_inflight: std::sync::Arc<dashmap::DashSet<u64>>,

    /// v0.6.2 §12.10 — set while the operator has declared `BULKMODE ON`
    /// (bulk seed/load). Suppresses per-record BatchIngest heat recording
    /// on the batch write paths: a bulk load carries no hot/cold signal
    /// (every record is loaded once, BatchIngest weight is 0.1) yet costs
    /// an O(N) `evict_coldest` scan per new bucket once the heat map
    /// saturates (`HEAT_MAP_CAP` vs ~client-cardinality gravity buckets).
    /// Deliberately decoupled from `compaction_enabled()`: that flag is
    /// also cleared by snapshots and ghost refreshes, where heat IS still
    /// meaningful and must keep being recorded.
    pub(crate) bulk_loading: std::sync::atomic::AtomicBool,

    /// 2b — per-anchor-shard write serialization. Sharded by `dict_key`
    /// hash so the uniqueness check + commit for one anchor value is atomic
    /// against concurrent same-anchor PUTs (closes the TOCTOU between
    /// `dictionary.get` and the batch commit). Different anchors hash to
    /// different shards and stay fully concurrent; single-stream bulk load
    /// sees no contention.
    pub(crate) anchor_shard_locks: Vec<parking_lot::Mutex<()>>,
}

mod boot;
mod dispatch;
mod ghosts;
mod gravity;
mod maintenance;
mod satellites;
mod vectors;
mod verbs;
/// Test-only MIGRATE crash-injection knobs (durability gate). No-ops in
/// production; re-exported so the durability test suite can drive them.
pub use maintenance::{FORCE_MIGRATE_ABORT_AFTER_WINDOWS, MIGRATE_WINDOW_LIMIT};
/// Test-only satellite bounded-scan knobs. No-ops in production; re-exported so
/// the satellite test suite can drive the route-equivalence and anti-collision
/// gates. See `engine::satellites`.
pub use satellites::{SAT_FORCE_PARENT_SCAN, SAT_SKIP_ANTICOLLISION_RESIDUAL};
mod lifecycle;
mod stats;

#[cfg(test)]
mod tests;

impl Engine {
    /// Public accessor for the underlying turba-engine handle. Used by
    /// integration tests that need the storage layer directly; production
    /// callers reach turba-engine through the query API.
    pub fn turba(&self) -> &std::sync::Arc<turba_engine::engine::TurbaEngine> {
        &self.turba
    }

    /// Path to the engine's `meta/` directory.
    pub fn meta_path(&self) -> &Path {
        &self.meta_path
    }

    /// 2b — acquire the anchor write-serialization shards for the given
    /// `dict_keys`, in a stable (sorted, deduped) order so two PUTs touching
    /// the same set of anchors can never deadlock against each other. Held by
    /// the caller across the uniqueness check + the batch commit so concurrent
    /// same-anchor PUTs serialize; distinct anchors map to distinct shards and
    /// proceed concurrently. Acquired AFTER any registry read and released
    /// after the commit — strictly outer to the keyspace memtable locks the
    /// commit takes, inner to the registries (see 8d lock-order analysis).
    pub(crate) fn lock_anchor_shards(
        &self,
        dict_keys: &[Vec<u8>],
    ) -> Vec<parking_lot::MutexGuard<'_, ()>> {
        use std::hash::{Hash, Hasher};
        let mut idxs: Vec<usize> = dict_keys
            .iter()
            .map(|k| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                k.hash(&mut h);
                (h.finish() as usize) % ANCHOR_SHARDS
            })
            .collect();
        idxs.sort_unstable();
        idxs.dedup();
        idxs.into_iter()
            .map(|i| self.anchor_shard_locks[i].lock())
            .collect()
    }

    /// Resolve the spatial Tree to read. The engine is single-tier, so
    /// this is a zero-overhead alias to the only spatial Tree.
    #[inline]
    pub(crate) fn spatial_tree(&self) -> std::sync::Arc<turba_engine::tree::Tree> {
        std::sync::Arc::clone(&self.turba.spatial)
    }

    /// The spatial Tree the source-lobe data lives on. Used by ghost
    /// creation (`GhostManager::create` / `create_batch`) which walks
    /// the full lobe. The engine is single-tier, so this is the one
    /// canonical spatial tree.
    pub(crate) fn ghost_spatial_tree(&self) -> std::sync::Arc<turba_engine::tree::Tree> {
        std::sync::Arc::clone(&self.turba.spatial)
    }

    /// Resolve lobe_id → lobe name. Used by ops to inject lobe_name on deserialization.
    pub(crate) fn lobe_name_for_id(&self, lobe_id: u16) -> String {
        self.lobe_registry
            .read()
            .get_by_id(lobe_id)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }

    /// Path to the clean-shutdown marker. Its presence at open means the
    /// previous shutdown flushed the (memtable-only) ghost index durably, so the
    /// persisted index is consistent and can be trusted; its absence means a
    /// crash (or the first boot).
    fn clean_shutdown_marker(&self) -> std::path::PathBuf {
        self.meta_path.join("clean_shutdown")
    }

    /// Best-effort directory fsync so a marker create/remove is itself durable.
    fn fsync_dir(dir: &std::path::Path) {
        if let Ok(f) = std::fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }

    /// Write `bytes` to `path` durably: write, fsync the file, then fsync the
    /// containing directory so the entry survives power loss (the 3g lesson).
    /// Plain `std::fs::write` leaves both the data and the directory entry in
    /// the page cache — they survive a process crash but not power loss.
    fn write_file_durable(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Some(dir) = path.parent() {
            Self::fsync_dir(dir);
        }
        Ok(())
    }

    /// Resolve a FindTarget to a list of matching records.
    pub(crate) fn resolve_find(
        &self,
        target: &xytalk_parser::ast::FindTarget,
        filters: &[xytalk_parser::ast::Filter],
    ) -> Result<Vec<(Record, [u8; xyzdb_core::key::SPATIAL_KEY_SIZE])>> {
        crate::ops::find::resolve_find_internal(self, target, filters)
    }

    /// Resolve a target + optional filter EXPRESSION (xyTalk v1 P1). An AND-pure
    /// predicate (or none) takes the EXACT same anchor/gravity fast path as
    /// [`Self::resolve_find`] — no regression for indexable queries. An OR/NOT
    /// predicate is not index-resolvable, so it resolves the target's candidate
    /// set unfiltered and applies the single filter walker (`matches_core_expr`)
    /// per record — the honest "OR ⇒ scan" cost contract, shared with SCAN.
    pub(crate) fn resolve_find_expr(
        &self,
        target: &xytalk_parser::ast::FindTarget,
        filter_expr: &Option<xytalk_parser::ast::FilterExpr>,
    ) -> Result<Vec<(Record, [u8; xyzdb_core::key::SPATIAL_KEY_SIZE])>> {
        match filter_expr {
            // No WHERE → same as resolve_find with no filters.
            None => self.resolve_find(target, &[]),
            Some(expr) => match expr.as_flat_and() {
                // Pure-AND → flatten and take the identical fast path.
                Some(flat) => {
                    let flat: Vec<xytalk_parser::ast::Filter> = flat.into_iter().cloned().collect();
                    self.resolve_find(target, &flat)
                }
                // OR/NOT → candidate set (unfiltered) + walker filter.
                None => {
                    let core = crate::ops::to_core_expr(expr);
                    let all = self.resolve_find(target, &[])?;
                    Ok(all
                        .into_iter()
                        .filter(|(r, _)| crate::ops::matches_core_expr(r, &core))
                        .collect())
                }
            },
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

trait RegistryLoad: Default + Sized {
    fn deserialize(data: &[u8]) -> Result<Self>;
}

impl RegistryLoad for LobeRegistry {
    fn deserialize(data: &[u8]) -> Result<Self> {
        LobeRegistry::from_bytes(data)
    }
}

impl RegistryLoad for AnchorRegistry {
    fn deserialize(data: &[u8]) -> Result<Self> {
        AnchorRegistry::from_bytes(data)
    }
}

fn load_or_default<T: RegistryLoad>(path: &Path) -> Result<T> {
    if path.exists() {
        let data = std::fs::read(path)
            .map_err(|e| XyzError::Storage(format!("failed to read {}: {e}", path.display())))?;
        T::deserialize(&data)
    } else {
        Ok(T::default())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Resource probes (shared by reap_cycle + stats_snapshot) ─────────────────
//
// Linux-only reads of `/proc/self/status` and cgroup v1/v2 memory accounting.
// Return MB (integer truncation) as that matches the format the reap-cycle
// log emits; `stats_snapshot` converts back to bytes for the JSON response.
// All probes return `None` on failure so callers can fall back to `0`.

#[cfg(target_os = "linux")]
fn read_proc_status_mb(field: &str) -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix(field).and_then(|r| r.strip_prefix(':')) {
            let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn read_proc_status_mb(_field: &str) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_cgroup_stat_mb(field: &str) -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.stat",
        "/sys/fs/cgroup/memory/memory.stat",
    ] {
        if let Ok(s) = std::fs::read_to_string(path) {
            for line in s.lines() {
                let mut parts = line.split_whitespace();
                if parts.next() == Some(field) {
                    if let Some(bytes) = parts.next().and_then(|v| v.parse::<u64>().ok()) {
                        return Some(bytes / (1024 * 1024));
                    }
                }
            }
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn read_cgroup_stat_mb(_field: &str) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_cgroup_limit_mb() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let v = s.trim();
            if v == "max" {
                continue;
            }
            if let Ok(bytes) = v.parse::<u64>() {
                return Some(bytes / (1024 * 1024));
            }
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn read_cgroup_limit_mb() -> Option<u64> {
    None
}
