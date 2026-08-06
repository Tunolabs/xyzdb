//! Memory budget: the single memory knob for the engine.
//!
//! Operators declare (or the environment implies) one number — a memory
//! budget in bytes — and every other memory sizing is derived from it.
//! Today the only derived consumer is the block cache, which takes a
//! quarter of the budget (`cache_bytes_from_budget`, floored at 32 MiB).
//! The remainder is the implicit streaming headroom: it is never
//! allocated up front, so no explicit "streaming budget" knob exists.
//!
//! ## Source precedence
//! 1. **Explicit** — `--memory-budget-mb` / `XYZDB_MEMORY_BUDGET_MB`
//!    (flag wins over env). The override for loose binaries, sidecar
//!    reservation, and benchmarks.
//! 2. **Cgroup** — the container / miniVM memory limit, read from the
//!    cgroup filesystem. Zero-config for the common deployment.
//! 3. **Default** — [`DEFAULT_BUDGET_BYTES`] (1 GiB), which derives to the
//!    historical 256 MB cache so the no-limit fallback preserves today's
//!    behaviour. The caller is expected to warn the operator in this case.
//!
//! ## Correctness note
//! Auto-detect reads the **cgroup limit**, never the machine's physical
//! RAM. Under a container the two differ — the host may have 128 GiB while
//! the container is capped at 2 GiB — and sizing the cache off physical
//! RAM would blow the container's limit and get the process OOM-killed.
//! This module therefore reads only the cgroup files and deliberately does
//! not touch `sysinfo`, `/proc/meminfo`, `sysconf`, or any other
//! physical-memory source.

// SPDX-License-Identifier: BUSL-1.1
use std::path::Path;

/// Conservative fallback budget used when no explicit override and no
/// cgroup limit are found: 1 GiB. Derives (via `cache_bytes_from_budget`)
/// to the historical 256 MB cache default, so a no-limit environment keeps
/// today's behaviour unchanged.
pub const DEFAULT_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

/// Absolute ceiling on the derived block cache, independent of how large the
/// budget is: 2 GiB. `budget/4` alone is unbounded — a 128 GiB container would
/// derive a 32 GiB cache, which contradicts the density goal (many small
/// instances per host) and buys nothing for this workload: the M1 flamegraph
/// showed the vector NEAREST scan is 0-hit in the block cache (doubling
/// 2 GiB→4 GiB changed nothing — a streaming scan retains no blocks), so cache
/// beyond a small point-get working set is wasted. 2 GiB is the tightest value
/// that stays behavior-preserving: it is exactly the cache the 8 GiB (T6) tier
/// already derived, so every shipped tier (≤ 8 GiB) is unchanged and only larger
/// single-container deployments are capped. Revisiting the `budget/4` FRACTION
/// (as opposed to this ceiling) is a separate, post-launch measurement.
const CACHE_CEILING_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Fraction of the budget available for in-memory memtables (active + sealed,
/// summed across all keyspaces): 35 %. The budget splits roughly 25 % block
/// cache ([`cache_bytes_from_budget`]) + 35 % memtables + 40 % headroom for
/// query scratch (NEAREST materialization), the allocator, the WAL, and the
/// transient SSTable build during a flush.
const MEMTABLE_BUDGET_PCT: u64 = 35;

/// Absolute ceiling on the derived memtable footprint, independent of budget:
/// 264 MiB. This is exactly today's worst case — 1 active + 2 sealed (the
/// compaction-on backpressure threshold) across the five production keyspaces
/// at their tuned SSD seal sizes: `(32 + 32 + 8 + 8 + 8) MiB × 3`. Capping here
/// makes every budget at or above ~755 MiB reproduce today's ingest behaviour
/// (the scale factor saturates at 1.0), so shipped tiers (≤ 8 GiB) are
/// unchanged and only tight budgets shrink.
const MEMTABLE_CEILING_CAP: u64 = 264 * 1024 * 1024;

/// Floor on the derived memtable footprint: 24 MiB. Below this, ingest cannot
/// make useful progress.
const MEMTABLE_CEILING_FLOOR: u64 = 24 * 1024 * 1024;

/// Per-keyspace seal-size floor: 2 MiB. A budget so tight that a scaled seal
/// would fall below this is clamped up — trading a little more resident memory
/// for avoiding a degenerate flood of micro-SSTables (write amplification).
const MIN_SEAL_BYTES: u64 = 2 * 1024 * 1024;

/// cgroup v1 reports "no limit" as a huge sentinel near `i64::MAX` rounded
/// down to a page boundary (commonly `0x7FFF_FFFF_FFFF_F000`). Any value at
/// or above this threshold is treated as "unlimited", not a real cap — no
/// real container is provisioned with exabytes of RAM.
const CGROUP_V1_UNLIMITED_THRESHOLD: u64 = 0x7000_0000_0000_0000;

/// Where a resolved budget came from. Surfaced for startup logging so the
/// operator can see whether the number was explicit, auto-detected, or the
/// conservative default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BudgetSource {
    /// From `--memory-budget-mb` or `XYZDB_MEMORY_BUDGET_MB`.
    Explicit,
    /// Auto-detected from the cgroup v2 unified hierarchy (`memory.max`).
    CgroupV2,
    /// Auto-detected from cgroup v1 (`memory/memory.limit_in_bytes`).
    CgroupV1,
    /// No override and no cgroup limit — [`DEFAULT_BUDGET_BYTES`].
    #[default]
    Default,
}

/// A resolved memory budget: the number of bytes and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedBudget {
    /// The budget in bytes.
    pub bytes: u64,
    /// Provenance of `bytes`.
    pub source: BudgetSource,
}

impl Default for ResolvedBudget {
    /// The conservative default budget ([`DEFAULT_BUDGET_BYTES`], source
    /// [`BudgetSource::Default`]). Used by internal open paths that do not
    /// resolve a budget of their own.
    fn default() -> Self {
        Self {
            bytes: DEFAULT_BUDGET_BYTES,
            source: BudgetSource::Default,
        }
    }
}

/// Resolve the effective memory budget from the source precedence.
///
/// # Arguments
/// * `explicit_mb` — the operator-supplied budget in MB, already merged
///   from the flag and the env var (flag wins) by the caller. `Some` short-
///   circuits every auto-detect step.
///
/// # Returns
/// The resolved budget in bytes plus its [`BudgetSource`]. Precedence:
/// explicit → cgroup (v2 then v1) → [`DEFAULT_BUDGET_BYTES`].
///
/// # Example
/// ```
/// use turba_engine::memory_budget::{resolve_memory_budget, BudgetSource};
/// let b = resolve_memory_budget(Some(2048));
/// assert_eq!(b.bytes, 2048 * 1024 * 1024);
/// assert_eq!(b.source, BudgetSource::Explicit);
/// ```
pub fn resolve_memory_budget(explicit_mb: Option<u64>) -> ResolvedBudget {
    if let Some(mb) = explicit_mb {
        return ResolvedBudget {
            bytes: mb * 1024 * 1024,
            source: BudgetSource::Explicit,
        };
    }
    if let Some((bytes, source)) = read_cgroup_limit() {
        return ResolvedBudget { bytes, source };
    }
    ResolvedBudget::default()
}

/// Derive the block-cache capacity from the memory budget: a quarter of the
/// budget, floored at 32 MiB (so tiny budgets still get a usable cache) and
/// capped at [`CACHE_CEILING_BYTES`] (2 GiB, so a huge container cannot derive a
/// runaway cache that starves density and would not help this workload anyway).
///
/// # Arguments
/// * `budget` — the memory budget in bytes.
///
/// # Returns
/// The block-cache size in bytes. `1 GiB → 256 MB` reproduces the historical
/// default; the 32 MiB floor guards very small budgets; the 2 GiB ceiling caps
/// budgets above 8 GiB (every shipped tier, ≤ 8 GiB, is unaffected).
///
/// # Example
/// ```
/// use turba_engine::memory_budget::cache_bytes_from_budget;
/// assert_eq!(cache_bytes_from_budget(8 * 1024 * 1024 * 1024), 2 * 1024 * 1024 * 1024);
/// assert_eq!(cache_bytes_from_budget(64 * 1024 * 1024), 32 * 1024 * 1024);
/// // Above 8 GiB the ceiling holds: a 128 GiB budget still caps at 2 GiB.
/// assert_eq!(cache_bytes_from_budget(128 * 1024 * 1024 * 1024), 2 * 1024 * 1024 * 1024);
/// ```
pub fn cache_bytes_from_budget(budget: u64) -> u64 {
    (budget / 4).clamp(32 * 1024 * 1024, CACHE_CEILING_BYTES)
}

/// Derive the global memtable footprint ceiling from the memory budget:
/// [`MEMTABLE_BUDGET_PCT`] % of the budget, floored at [`MEMTABLE_CEILING_FLOOR`]
/// and capped at [`MEMTABLE_CEILING_CAP`].
///
/// This is the hard ceiling the ingest backpressure enforces on the sum of
/// every keyspace's active + sealed memtable bytes: writes stall (waiting for
/// flush to drain) when the sum reaches it. Deriving it from the budget — not a
/// fixed constant — is what lets a tight container (e.g. 128 MiB) bound its own
/// ingest instead of OOM-ing while building a large index.
///
/// # Arguments
/// * `budget` — the memory budget in bytes.
///
/// # Returns
/// The memtable ceiling in bytes, in `[MEMTABLE_CEILING_FLOOR, MEMTABLE_CEILING_CAP]`.
///
/// # Example
/// ```
/// use turba_engine::memory_budget::memtable_ceiling_from_budget;
/// // Budgets >= ~755 MiB saturate at the cap (today's worst case, unchanged).
/// assert_eq!(memtable_ceiling_from_budget(8 * 1024 * 1024 * 1024), 264 * 1024 * 1024);
/// // Very tight budgets hit the floor (35 % of 16 MiB < 24 MiB).
/// assert_eq!(memtable_ceiling_from_budget(16 * 1024 * 1024), 24 * 1024 * 1024);
/// // In between, 35 % of the budget.
/// assert!(memtable_ceiling_from_budget(256 * 1024 * 1024) < 264 * 1024 * 1024);
/// ```
pub fn memtable_ceiling_from_budget(budget: u64) -> u64 {
    (budget * MEMTABLE_BUDGET_PCT / 100).clamp(MEMTABLE_CEILING_FLOOR, MEMTABLE_CEILING_CAP)
}

/// The multiplier (≤ 1.0) applied to each keyspace's tuned seal size so their
/// summed worst case fits [`memtable_ceiling_from_budget`]. Equals the ceiling
/// over [`MEMTABLE_CEILING_CAP`] (today's worst case), so it is `1.0` for any
/// budget at or above ~755 MiB (seal sizes then match today exactly) and shrinks
/// below that. Profile-independent: it scales whatever seal sizes a storage
/// profile defines by the same ratio.
///
/// # Arguments
/// * `budget` — the memory budget in bytes.
///
/// # Returns
/// A factor in `(0.0, 1.0]`.
pub fn memtable_scale_factor(budget: u64) -> f64 {
    memtable_ceiling_from_budget(budget) as f64 / MEMTABLE_CEILING_CAP as f64
}

/// Scale a keyspace's production seal size down to fit the budget, never below
/// [`MIN_SEAL_BYTES`]. At budgets ≥ ~755 MiB the factor is `1.0` and the size is
/// returned unchanged (downward-only — big budgets are byte-for-byte today).
///
/// # Arguments
/// * `prod_seal_bytes` — the keyspace's tuned seal size at full budget.
/// * `budget` — the memory budget in bytes.
///
/// # Returns
/// The budget-scaled seal size in bytes.
///
/// # Example
/// ```
/// use turba_engine::memory_budget::scale_seal_size;
/// // 8 GiB: unchanged.
/// assert_eq!(scale_seal_size(32 * 1024 * 1024, 8 * 1024 * 1024 * 1024), 32 * 1024 * 1024);
/// // 128 MiB: a small keyspace hits the 2 MiB floor.
/// assert_eq!(scale_seal_size(8 * 1024 * 1024, 128 * 1024 * 1024), 2 * 1024 * 1024);
/// ```
pub fn scale_seal_size(prod_seal_bytes: u64, budget: u64) -> u64 {
    let scaled = (prod_seal_bytes as f64 * memtable_scale_factor(budget)) as u64;
    scaled.max(MIN_SEAL_BYTES)
}

/// Read the cgroup memory limit from the well-known cgroup filesystem
/// paths. Returns `None` when neither hierarchy exposes a real cap (not in
/// a container, or the container is unconstrained).
fn read_cgroup_limit() -> Option<(u64, BudgetSource)> {
    read_cgroup_limit_from(
        Path::new("/sys/fs/cgroup/memory.max"),
        Path::new("/sys/fs/cgroup/memory/memory.limit_in_bytes"),
    )
}

/// Read the cgroup memory limit from injectable paths (testable core of
/// [`read_cgroup_limit`]).
///
/// # Arguments
/// * `v2_path` — cgroup v2 `memory.max` (unified hierarchy).
/// * `v1_path` — cgroup v1 `memory/memory.limit_in_bytes`.
///
/// # Returns
/// * `Some((bytes, CgroupV2))` when v2 holds a numeric cap.
/// * `Some((bytes, CgroupV1))` when v2 is absent/`"max"` and v1 holds a
///   real cap (below [`CGROUP_V1_UNLIMITED_THRESHOLD`]).
/// * `None` when neither yields a real limit.
///
/// v2 `"max"` (unlimited) and a missing/unreadable v2 file both fall
/// through to v1. Reads only these two files — never physical RAM.
fn read_cgroup_limit_from(v2_path: &Path, v1_path: &Path) -> Option<(u64, BudgetSource)> {
    if let Some(bytes) = read_cgroup_v2(v2_path) {
        return Some((bytes, BudgetSource::CgroupV2));
    }
    read_cgroup_v1(v1_path).map(|bytes| (bytes, BudgetSource::CgroupV1))
}

/// Parse a cgroup v2 `memory.max`. `None` on a missing/unreadable file, on
/// the `"max"` (unlimited) sentinel, or on unparseable contents — all of
/// which fall through to the v1 path.
fn read_cgroup_v2(path: &Path) -> Option<u64> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if trimmed == "max" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// Parse a cgroup v1 `memory.limit_in_bytes`. `None` on a
/// missing/unreadable/unparseable file or on the unlimited sentinel
/// (value at or above [`CGROUP_V1_UNLIMITED_THRESHOLD`]).
fn read_cgroup_v1(path: &Path) -> Option<u64> {
    let contents = std::fs::read_to_string(path).ok()?;
    let value = contents.trim().parse::<u64>().ok()?;
    if value >= CGROUP_V1_UNLIMITED_THRESHOLD {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn explicit_wins() {
        let b = resolve_memory_budget(Some(4096));
        assert_eq!(b.bytes, 4096 * 1024 * 1024);
        assert_eq!(b.source, BudgetSource::Explicit);
    }

    #[test]
    fn cgroup_v2_number() {
        let dir = tempdir().unwrap();
        let v2 = dir.path().join("memory.max");
        fs::write(&v2, "8589934592\n").unwrap();
        let missing_v1 = dir.path().join("nope.limit_in_bytes");

        let got = read_cgroup_limit_from(&v2, &missing_v1);
        assert_eq!(got, Some((8589934592, BudgetSource::CgroupV2)));
    }

    #[test]
    fn cgroup_v2_max_falls_through() {
        let dir = tempdir().unwrap();
        let v2 = dir.path().join("memory.max");
        let v1 = dir.path().join("memory.limit_in_bytes");
        fs::write(&v2, "max\n").unwrap();
        fs::write(&v1, "536870912\n").unwrap();

        let got = read_cgroup_limit_from(&v2, &v1);
        assert_eq!(got, Some((536870912, BudgetSource::CgroupV1)));
    }

    #[test]
    fn cgroup_v1_unlimited_sentinel() {
        let dir = tempdir().unwrap();
        let v1 = dir.path().join("memory.limit_in_bytes");
        fs::write(&v1, "9223372036854771712\n").unwrap();
        let missing_v2 = dir.path().join("nope.max");

        let got = read_cgroup_limit_from(&missing_v2, &v1);
        assert_eq!(got, None);
    }

    #[test]
    fn no_cgroup_returns_default() {
        let dir = tempdir().unwrap();
        let missing_v2 = dir.path().join("nope.max");
        let missing_v1 = dir.path().join("nope.limit_in_bytes");

        // The helper yields nothing when neither hierarchy exists...
        assert_eq!(read_cgroup_limit_from(&missing_v2, &missing_v1), None);
        // ...and the conservative fallback is 1 GiB.
        assert_eq!(DEFAULT_BUDGET_BYTES, 1024 * 1024 * 1024);
        assert_eq!(ResolvedBudget::default().bytes, DEFAULT_BUDGET_BYTES);
        assert_eq!(ResolvedBudget::default().source, BudgetSource::Default);
    }

    #[test]
    fn derivation() {
        // Every shipped tier (<= 8 GiB) is budget/4, unchanged by the ceiling.
        assert_eq!(cache_bytes_from_budget(256 * 1024 * 1024), 64 * 1024 * 1024);
        assert_eq!(
            cache_bytes_from_budget(2 * 1024 * 1024 * 1024),
            512 * 1024 * 1024
        );
        assert_eq!(
            cache_bytes_from_budget(8 * 1024 * 1024 * 1024),
            2 * 1024 * 1024 * 1024
        );
        // Floor: tiny budgets still get 32 MiB.
        assert_eq!(cache_bytes_from_budget(64 * 1024 * 1024), 32 * 1024 * 1024);
    }

    #[test]
    fn ceiling_caps_large_budgets() {
        // Above 8 GiB, budget/4 would run away (16 GiB, 32 GiB); the 2 GiB
        // ceiling holds so a big single container cannot starve density.
        assert_eq!(
            cache_bytes_from_budget(64 * 1024 * 1024 * 1024),
            CACHE_CEILING_BYTES
        );
        assert_eq!(
            cache_bytes_from_budget(128 * 1024 * 1024 * 1024),
            2 * 1024 * 1024 * 1024
        );
        // The 8 GiB tier sits exactly at the ceiling — the boundary is inclusive
        // and behavior-preserving (still 2 GiB, not capped below).
        assert_eq!(
            cache_bytes_from_budget(8 * 1024 * 1024 * 1024),
            CACHE_CEILING_BYTES
        );
    }

    #[test]
    fn memtable_ceiling_saturates_and_floors() {
        // >= ~755 MiB saturates at the cap (today's worst case, unchanged).
        assert_eq!(
            memtable_ceiling_from_budget(8 * 1024 * 1024 * 1024),
            MEMTABLE_CEILING_CAP
        );
        assert_eq!(
            memtable_ceiling_from_budget(1024 * 1024 * 1024),
            MEMTABLE_CEILING_CAP
        );
        // Tight budgets take 35 %.
        assert_eq!(
            memtable_ceiling_from_budget(256 * 1024 * 1024),
            256 * 1024 * 1024 * MEMTABLE_BUDGET_PCT / 100
        );
        // Very tight budgets hit the floor (35 % of 16 MiB < 24 MiB).
        assert_eq!(
            memtable_ceiling_from_budget(16 * 1024 * 1024),
            MEMTABLE_CEILING_FLOOR
        );
        // Monotonic non-decreasing in the budget.
        let mut prev = 0;
        for mb in [64u64, 128, 256, 512, 768, 1024, 8192] {
            let c = memtable_ceiling_from_budget(mb * 1024 * 1024);
            assert!(c >= prev, "ceiling must be monotonic (mb={mb})");
            prev = c;
        }
    }

    #[test]
    fn scale_factor_is_downward_only() {
        // Never above 1.0, for any budget.
        for mb in [64u64, 128, 256, 512, 768, 1024, 8192, 65536] {
            assert!(
                memtable_scale_factor(mb * 1024 * 1024) <= 1.0,
                "factor must be <= 1.0 (mb={mb})"
            );
        }
        // Exactly 1.0 at/above the cap threshold — shipped tiers unchanged.
        assert_eq!(memtable_scale_factor(1024 * 1024 * 1024), 1.0);
        assert_eq!(memtable_scale_factor(8 * 1024 * 1024 * 1024), 1.0);
    }

    #[test]
    fn seal_size_preserves_today_and_floors_tight() {
        // 8 GiB: every production seal size returned unchanged (byte-for-byte today).
        for prod_mib in [32u64, 8] {
            let prod = prod_mib * 1024 * 1024;
            assert_eq!(scale_seal_size(prod, 8 * 1024 * 1024 * 1024), prod);
        }
        // 128 MiB: large keyspaces shrink; small ones hit the 2 MiB floor.
        assert!(scale_seal_size(32 * 1024 * 1024, 128 * 1024 * 1024) < 32 * 1024 * 1024);
        assert_eq!(
            scale_seal_size(8 * 1024 * 1024, 128 * 1024 * 1024),
            MIN_SEAL_BYTES
        );
        // Never below the floor, however tiny the budget.
        assert_eq!(scale_seal_size(1, 8 * 1024 * 1024), MIN_SEAL_BYTES);
    }
}
